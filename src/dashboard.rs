use crate::{cache::App, protocols::escape, response, Error};
use axum::{extract::State, http::StatusCode, response::Response, Json};
use serde::Serialize;
use std::sync::{atomic::Ordering, Arc};

#[derive(Serialize, sqlx::FromRow)]
struct RegistryStats {
    registry: String,
    entries: i64,
    bytes: i64,
}
#[derive(Serialize, sqlx::FromRow)]
struct Event {
    time: i64,
    registry: String,
    path: String,
    outcome: String,
}
#[derive(Serialize, sqlx::FromRow)]
struct Upstream {
    registry: String,
    checked_at: i64,
    healthy: bool,
    status: String,
}
#[derive(Serialize)]
pub struct Snapshot {
    version: &'static str,
    hits: u64,
    misses: u64,
    stale: u64,
    failures: u64,
    blob_bytes: i64,
    limit_bytes: u64,
    registries: Vec<RegistryStats>,
    upstreams: Vec<Upstream>,
    recent: Vec<Event>,
}

async fn snapshot(app: &App) -> Result<Snapshot, Error> {
    Ok(Snapshot {
        version: env!("CARGO_PKG_VERSION"), hits: app.hits.load(Ordering::Relaxed), misses: app.misses.load(Ordering::Relaxed), stale: app.stale.load(Ordering::Relaxed), failures: app.failures.load(Ordering::Relaxed),
        blob_bytes: sqlx::query_scalar("SELECT COALESCE(SUM(size),0) FROM (SELECT digest,MAX(size) AS size FROM entries GROUP BY digest)").fetch_one(&app.db).await.map_err(anyhow::Error::from)?,
        limit_bytes: app.config.cache.max_size_bytes,
        registries: sqlx::query_as("SELECT registry,COUNT(*) AS entries,SUM(size) AS bytes FROM entries GROUP BY registry").fetch_all(&app.db).await.map_err(anyhow::Error::from)?,
        upstreams: sqlx::query_as("SELECT * FROM upstream_status ORDER BY registry").fetch_all(&app.db).await.map_err(anyhow::Error::from)?,
        recent: sqlx::query_as("SELECT time,registry,path,outcome FROM events ORDER BY id DESC LIMIT 50").fetch_all(&app.db).await.map_err(anyhow::Error::from)?,
    })
}

pub async fn status(State(app): State<Arc<App>>) -> Result<Json<Snapshot>, Error> {
    Ok(Json(snapshot(&app).await?))
}

pub async fn metrics(State(app): State<Arc<App>>) -> Result<Response, Error> {
    let s = snapshot(&app).await?;
    let body = format!("# TYPE nexup_cache_hits_total counter\nnexup_cache_hits_total {}\n# TYPE nexup_cache_misses_total counter\nnexup_cache_misses_total {}\n# TYPE nexup_stale_total counter\nnexup_stale_total {}\n# TYPE nexup_upstream_failures_total counter\nnexup_upstream_failures_total {}\n# TYPE nexup_blob_bytes gauge\nnexup_blob_bytes {}\n", s.hits,s.misses,s.stale,s.failures,s.blob_bytes);
    Ok(response(StatusCode::OK, "text/plain; version=0.0.4", body))
}

pub async fn dashboard(State(app): State<Arc<App>>) -> Result<Response, Error> {
    let s = snapshot(&app).await?;
    let mb = |bytes: u64| format!("{:.1} MiB", bytes as f64 / 1048576.0);
    let mut body = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta http-equiv="refresh" content="30"><title>Nexup · Cache</title><style>
:root{{color-scheme:dark;font:15px system-ui;background:#10151d;color:#e4edf5}}body{{max-width:1120px;margin:40px auto;padding:0 24px}}header{{display:flex;justify-content:space-between;align-items:center}}h1{{font-size:30px;margin-bottom:4px}}h2{{font-size:18px;margin-top:32px}}p,small{{color:#93a6b9}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:16px;margin:28px 0}}.card{{background:#192330;border:1px solid #2a394a;border-radius:12px;padding:20px}}.number{{font-size:28px;display:block;margin-top:10px}}table{{width:100%;border-collapse:collapse;font-size:14px}}th,td{{text-align:left;padding:12px;border-bottom:1px solid #263444}}th{{color:#93a6b9}}.path{{overflow-wrap:anywhere}}a{{color:#62d9bd}}.badge{{color:#62d9bd}}.scroll{{overflow:auto}}code{{color:#a2d9ff}}</style></head><body><header><div><h1>Nexup <span class="badge">↗</span></h1><p>Pull-through artifact cache · v{}</p></div><a href="/api/status">JSON status</a></header><section class="grid"><div class="card">Stored blobs<span class="number">{}</span></div><div class="card">Cache hits<span class="number">{}</span></div><div class="card">Upstream fetches<span class="number">{}</span></div><div class="card">Stale / errors<span class="number">{} / {}</span></div></section><p>Storage target: {} · Counters since startup · Refreshes every 30 seconds</p><h2>Registries</h2><div class="scroll"><table><tr><th>Registry</th><th>Enabled</th><th>Cached objects</th><th>Logical bytes</th><th>Last upstream result</th></tr>"#,
        s.version,
        mb(s.blob_bytes as u64),
        s.hits,
        s.misses,
        s.stale,
        s.failures,
        if s.limit_bytes == 0 {
            "unlimited".into()
        } else {
            mb(s.limit_bytes)
        }
    );
    for (name, enabled) in [
        ("docker", app.config.docker.enabled),
        ("pypi", app.config.pypi.enabled),
        ("npm", app.config.npm.enabled),
    ] {
        let stats = s.registries.iter().find(|r| r.registry == name);
        let health = s
            .upstreams
            .iter()
            .find(|u| u.registry == name)
            .map(|u| format!("{} (Unix {})", u.status, u.checked_at))
            .unwrap_or_else(|| "Not contacted yet".into());
        body.push_str(&format!(
            "<tr><td><a href=\"/ui/registries/{name}\">{name}</a></td><td>{enabled}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            stats.map_or(0, |s| s.entries),
            mb(stats.map_or(0, |s| s.bytes) as u64),
            escape(&health)
        ));
    }
    body.push_str("</table></div><h2>Recent downloads &amp; errors</h2><div class=\"scroll\"><table><tr><th>Time (Unix)</th><th>Registry</th><th>Artifact</th><th>Result</th></tr>");
    for e in s.recent {
        body.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=\"path\">{}</td><td>{}</td></tr>",
            e.time,
            escape(&e.registry),
            escape(&e.path),
            escape(&e.outcome)
        ));
    }
    body.push_str("</table></div><h2>Cached objects</h2><p>Most recently accessed, up to 100. Digests identify bytes shared across registry entries.</p><div class=\"scroll\"><table><tr><th>Registry</th><th>Artifact path</th><th>SHA-256</th><th>Size</th><th>Type</th></tr>");
    let entries = sqlx::query_as::<_, crate::cache::Entry>(
        "SELECT * FROM entries ORDER BY accessed_at DESC LIMIT 100",
    )
    .fetch_all(&app.db)
    .await
    .map_err(anyhow::Error::from)?;
    for e in entries {
        body.push_str(&format!(
            "<tr><td>{}</td><td class=\"path\">{}</td><td class=\"path\"><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            escape(&e.registry),
            escape(&e.path),
            escape(&e.digest),
            mb(e.size as u64),
            escape(&e.content_type)
        ));
    }
    body.push_str("</table></div><p>Read-only dashboard · <a href=\"/metrics\">Prometheus metrics</a> · <a href=\"/health\">Health</a></p></body></html>");
    let mut r = response(StatusCode::OK, "text/html; charset=utf-8", body);
    r.headers_mut().insert(
        "content-security-policy",
        "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    r.headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    Ok(r)
}

pub(crate) fn size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / 1048576.0)
    } else {
        format!("{:.2} GiB", bytes as f64 / 1073741824.0)
    }
}

pub(crate) fn timestamp(seconds: i64) -> String {
    if !(0..253402300800).contains(&seconds) {
        return "Unknown".into();
    }
    httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds as u64))
}

pub(crate) fn page(title: &str, active: &str, content: String) -> Response {
    let mut html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{} · Nexup</title><style>
:root{{color-scheme:dark;font:15px system-ui;background:#10151d;color:#e4edf5}}body{{max-width:1120px;margin:40px auto;padding:0 24px}}header{{display:flex;justify-content:space-between;align-items:center}}.brand{{font-size:28px;font-weight:700;text-decoration:none;color:#e4edf5}}h1{{font-size:30px;overflow-wrap:anywhere}}h2{{font-size:18px;margin-top:32px}}p,small{{color:#93a6b9;line-height:1.6}}nav{{display:flex;flex-wrap:wrap;gap:8px;border-bottom:1px solid #263444;margin:24px 0;padding-bottom:16px}}nav a{{padding:9px 16px;border-radius:8px;text-decoration:none}}nav a[aria-current=page]{{background:#193d38;color:#80ecd0}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:16px;margin:28px 0}}.card{{background:#192330;border:1px solid #2a394a;border-radius:12px;padding:20px}}.number{{font-size:28px;display:block;margin-top:10px}}table{{width:100%;border-collapse:collapse;font-size:14px}}th,td{{text-align:left;padding:12px;border-bottom:1px solid #263444;vertical-align:top}}th{{color:#93a6b9}}.path,dd{{overflow-wrap:anywhere}}a{{color:#62d9bd}}.scroll{{overflow:auto}}code{{color:#a2d9ff}}dl{{display:grid;grid-template-columns:minmax(100px,180px) 1fr;gap:12px}}dt{{color:#93a6b9}}dd{{margin:0;white-space:pre-wrap}}input,button{{font:inherit;padding:10px 14px;border-radius:8px;border:1px solid #42566b;background:#192330;color:#e4edf5}}input{{width:min(500px,70%)}}button{{cursor:pointer;background:#193d38}}.search{{display:flex;gap:10px;margin:8px 0}}.pagination{{display:flex;gap:20px;margin-top:24px}}a:focus-visible,input:focus-visible,button:focus-visible{{outline:2px solid #62d9bd;outline-offset:3px}}@media(max-width:600px){{body{{margin-top:20px;padding:0 16px}}nav a{{padding:8px}}dl{{grid-template-columns:1fr}}}}
</style></head><body><header><a class="brand" href="/">Nexup ↗</a><small>Pull-through artifact cache</small></header><nav aria-label="Main navigation">"#,
        escape(title)
    );
    for (key, label, href) in [
        ("overview", "Overview", "/"),
        ("docker", "Docker", "/ui/registries/docker"),
        ("pypi", "PyPI", "/ui/registries/pypi"),
        ("npm", "npm", "/ui/registries/npm"),
    ] {
        html.push_str(&format!(
            "<a href=\"{href}\"{}>{label}</a>",
            if key == active {
                " aria-current=\"page\""
            } else {
                ""
            }
        ));
    }
    html.push_str("</nav><main>");
    html.push_str(&content);
    html.push_str("</main><p>Read-only cache browser · <a href=\"/metrics\">Metrics</a> · <a href=\"/health\">Health</a></p></body></html>");
    let mut response = response(StatusCode::OK, "text/html; charset=utf-8", html);
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response.headers_mut().insert("content-security-policy","default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'".parse().unwrap());
    response
}
