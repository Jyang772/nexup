//! Read-only views derived from the cache. Browsing never contacts an upstream.
use crate::{
    cache::{App, Entry},
    dashboard,
    protocols::escape,
    Error,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Response,
};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use url::Url;

const PAGE_SIZE: usize = 50;

#[derive(Default, Deserialize)]
pub struct Filter {
    #[serde(default)]
    q: String,
    #[serde(default)]
    page: usize,
}

fn title(registry: &str) -> Result<&'static str, Error> {
    match registry {
        "docker" => Ok("Docker images"),
        "npm" => Ok("npm packages"),
        "pypi" => Ok("PyPI packages"),
        _ => Err(Error(StatusCode::NOT_FOUND, "unknown registry".into())),
    }
}

fn decode(path: &str) -> String {
    percent_decode_str(path).decode_utf8_lossy().into_owned()
}

fn normalize_python(name: &str) -> String {
    let mut normalized = String::new();
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !normalized.ends_with('-') {
                normalized.push('-');
            }
        } else {
            normalized.push(c.to_ascii_lowercase());
        }
    }
    normalized
}

fn distribution(filename: &str) -> Option<(String, String)> {
    let filename = filename.strip_suffix(".metadata").unwrap_or(filename);
    if let Some(wheel) = filename.strip_suffix(".whl") {
        let mut parts = wheel.split('-');
        return Some((normalize_python(parts.next()?), parts.next()?.to_owned()));
    }
    for extension in [".tar.gz", ".tar.bz2", ".tar.xz", ".zip", ".tgz"] {
        if let Some(stem) = filename.strip_suffix(extension) {
            let (name, version) = stem.rsplit_once('-')?;
            return Some((normalize_python(name), version.into()));
        }
    }
    None
}

fn relative_path(upstream: &str, path: &str) -> Option<String> {
    let base = Url::parse(upstream).ok()?;
    let prefix = format!("{}/", decode(base.path()).trim_end_matches('/'));
    decode(path).strip_prefix(&prefix).map(str::to_owned)
}

fn identity(app: &App, entry: &Entry) -> Option<String> {
    match entry.registry.as_str() {
        "docker" => {
            let path = relative_path(&app.config.docker.upstream, &entry.path)?;
            let repo = path.strip_prefix("v2/")?;
            repo.rsplit_once("/manifests/")
                .or_else(|| repo.rsplit_once("/blobs/"))
                .map(|(name, _)| name.to_owned())
        }
        "npm" => {
            let path = relative_path(&app.config.npm.upstream, &entry.path)?;
            let mut parts = path.split('/');
            let first = parts.next()?;
            if first.starts_with('@') {
                Some(format!("{first}/{}", parts.next()?))
            } else if !first.is_empty() && first != "-" {
                Some(first.into())
            } else {
                None
            }
        }
        "pypi" => {
            if let Some(path) = relative_path(&app.config.pypi.upstream, &entry.path) {
                let project = path.trim_end_matches('/');
                if !project.is_empty() && !project.contains('/') {
                    return Some(normalize_python(project));
                }
            }
            distribution(decode(&entry.path).rsplit('/').next()?).map(|(name, _)| name)
        }
        _ => None,
    }
}

pub(crate) fn package_url(registry: &str, name: &str) -> String {
    let mut url = Url::parse("http://localhost/ui/registries/").unwrap();
    {
        let mut segments = url.path_segments_mut().unwrap();
        segments.pop_if_empty().push(registry);
        for segment in name.split('/') {
            segments.push(segment);
        }
    }
    url.path().to_owned()
}

async fn entries(app: &App, registry: &str) -> Result<Vec<Entry>, Error> {
    Ok(
        sqlx::query_as("SELECT * FROM entries WHERE registry = ? ORDER BY accessed_at DESC")
            .bind(registry)
            .fetch_all(&app.db)
            .await
            .map_err(anyhow::Error::from)?,
    )
}

fn unique_bytes(entries: &[&Entry]) -> u64 {
    let mut seen = BTreeSet::new();
    entries
        .iter()
        .filter(|e| seen.insert(&e.digest))
        .map(|e| e.size.max(0) as u64)
        .sum()
}

pub async fn registry(
    State(app): State<Arc<App>>,
    Path(registry): Path<String>,
    Query(filter): Query<Filter>,
) -> Result<Response, Error> {
    let title = title(&registry)?;
    let all = entries(&app, &registry).await?;
    let mut packages: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    let mut unclassified = 0;
    for entry in &all {
        if let Some(name) = identity(&app, entry) {
            packages.entry(name).or_default().push(entry);
        } else {
            unclassified += 1;
        }
    }
    let total = packages.len();
    let query = filter.q.trim().to_lowercase();
    let matches: Vec<_> = packages
        .iter()
        .filter(|(name, _)| name.to_lowercase().contains(&query))
        .collect();
    let page = filter.page.min(matches.len().saturating_sub(1) / PAGE_SIZE);
    let mut html = format!("<h1>{title}</h1><p>{total} cached images/packages · {} matching · {unclassified} unclassified objects</p><form method=\"get\"><label for=\"q\">Find by name</label><div class=\"search\"><input id=\"q\" name=\"q\" value=\"{}\" placeholder=\"Search cached names\"><button>Search</button></div></form><p>Only locally cached content is shown. Browsing does not download or refresh packages.</p><div class=\"scroll\"><table><tr><th>Name</th><th>Cached objects</th><th>Unique blob size</th><th>Last accessed (UTC)</th></tr>", matches.len(), escape(&filter.q));
    for (name, entries) in matches.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
        html.push_str(&format!(
            "<tr><td><a href=\"{}\">{}</a></td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&package_url(&registry, name)),
            escape(name),
            entries.len(),
            dashboard::size(unique_bytes(entries)),
            dashboard::timestamp(entries.iter().map(|e| e.accessed_at).max().unwrap_or(0))
        ));
    }
    if matches.is_empty() {
        html.push_str("<tr><td colspan=\"4\">No matching cached images or packages yet.</td></tr>");
    }
    html.push_str("</table></div><div class=\"pagination\">");
    for (label, target) in [
        ("Previous", page.checked_sub(1)),
        (
            "Next",
            ((page + 1) * PAGE_SIZE < matches.len()).then_some(page + 1),
        ),
    ] {
        if let Some(target) = target {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("q", &filter.q)
                .append_pair("page", &target.to_string())
                .finish();
            html.push_str(&format!("<a href=\"?{}\">{label}</a> ", escape(&query)));
        }
    }
    html.push_str(&format!(
        "<span>Page {} of {}</span></div>",
        page + 1,
        matches.len().div_ceil(PAGE_SIZE).max(1)
    ));
    Ok(dashboard::page(title, &registry, html))
}

// A blob may have been evicted between the index query and this read. Treat that
// as unavailable metadata, not an upstream fetch or a failed catalog page.
async fn metadata(app: &App, entry: &Entry) -> Option<Vec<u8>> {
    if entry.size < 0 || entry.size as u64 > app.config.cache.max_metadata_bytes {
        return None;
    }
    tokio::fs::read(app.blob_path(&entry.digest)).await.ok()
}

fn text(value: Option<&Value>) -> String {
    value
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default()
}

fn info(html: &mut String, label: &str, value: &str) {
    if !value.is_empty() {
        html.push_str(&format!(
            "<dt>{}</dt><dd>{}</dd>",
            escape(label),
            escape(value)
        ));
    }
}

fn asset_path(app: &App, registry: &str, entry: &Entry, href: &str) -> Option<String> {
    let upstream = if registry == "npm" {
        &app.config.npm.upstream
    } else {
        &app.config.pypi.upstream
    };
    let base = Url::parse(upstream).ok()?.join(&entry.path).ok()?;
    Some(decode(base.join(href).ok()?.path()))
}

#[derive(Default)]
struct Release {
    version: String,
    filename: String,
    path: String,
    requires: String,
    integrity: String,
    notes: String,
}

fn npm_metadata(app: &App, entry: &Entry, value: &Value, releases: &mut BTreeMap<String, Release>) {
    if let Some(versions) = value.get("versions").and_then(Value::as_object) {
        for (version, value) in versions {
            npm_version(app, entry, version, value, releases);
        }
    } else if let Some(version) = value.get("version").and_then(Value::as_str) {
        npm_version(app, entry, version, value, releases);
    }
}

fn npm_version(
    app: &App,
    entry: &Entry,
    version: &str,
    value: &Value,
    releases: &mut BTreeMap<String, Release>,
) {
    let path = value
        .pointer("/dist/tarball")
        .and_then(Value::as_str)
        .and_then(|href| asset_path(app, "npm", entry, href))
        .unwrap_or_default();
    releases.entry(version.into()).or_insert_with(|| Release {
        version: version.into(),
        filename: path.rsplit('/').next().unwrap_or("").into(),
        path,
        requires: text(value.get("engines")),
        integrity: text(
            value
                .pointer("/dist/integrity")
                .or_else(|| value.pointer("/dist/shasum")),
        ),
        notes: text(value.get("deprecated")),
    });
}

fn pypi_metadata(app: &App, entry: &Entry, bytes: &[u8], releases: &mut BTreeMap<String, Release>) {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        if let Some(files) = value.get("files").and_then(Value::as_array) {
            for file in files {
                let filename = text(file.get("filename"));
                let path = file
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|href| asset_path(app, "pypi", entry, href))
                    .unwrap_or_default();
                let yanked = file
                    .get("yanked")
                    .filter(|v| !v.is_null() && **v != Value::Bool(false));
                releases.entry(filename.clone()).or_insert_with(|| Release {
                    version: distribution(&filename).map(|(_, v)| v).unwrap_or_default(),
                    filename,
                    path,
                    requires: text(file.get("requires-python")),
                    integrity: text(file.get("hashes")),
                    notes: yanked
                        .map(|v| format!("Yanked: {}", text(Some(v))))
                        .unwrap_or_default(),
                });
            }
        }
    } else {
        let document = scraper::Html::parse_document(&String::from_utf8_lossy(bytes));
        let selector = scraper::Selector::parse("a[href]").unwrap();
        for link in document.select(&selector) {
            let href = link.value().attr("href").unwrap();
            let path = asset_path(app, "pypi", entry, href).unwrap_or_default();
            let filename = path.rsplit('/').next().unwrap_or("").to_owned();
            if distribution(&filename).is_none() {
                continue;
            }
            releases.entry(filename.clone()).or_insert_with(|| Release {
                version: distribution(&filename).map(|(_, v)| v).unwrap_or_default(),
                filename,
                path,
                requires: link
                    .value()
                    .attr("data-requires-python")
                    .unwrap_or("")
                    .into(),
                integrity: href.split_once('#').map(|(_, h)| h).unwrap_or("").into(),
                notes: link
                    .value()
                    .attr("data-yanked")
                    .map(|s| format!("Yanked: {s}"))
                    .unwrap_or_default(),
            });
        }
    }
}

pub async fn package(
    State(app): State<Arc<App>>,
    Path((registry, name)): Path<(String, String)>,
) -> Result<Response, Error> {
    let registry_title = title(&registry)?;
    let all = entries(&app, &registry).await?;
    let mut members: Vec<&Entry> = all
        .iter()
        .filter(|e| identity(&app, e).as_deref() == Some(&name))
        .collect();
    if members.is_empty() {
        return Err(Error(
            StatusCode::NOT_FOUND,
            "image/package is not cached".into(),
        ));
    }
    let mut html = format!("<p><a href=\"/ui/registries/{registry}\">← {registry_title}</a></p><h1>{}</h1><p>Information from the local cache. Listed upstream versions may not have their artifact bytes cached.</p>",escape(&name));
    let mut details = String::new();
    let mut releases = BTreeMap::new();
    let mut seen_metadata = BTreeSet::new();
    let mut manifests = String::new();
    let mut unavailable = 0;
    for entry in &members {
        let is_manifest = registry == "docker" && entry.path.contains("/manifests/");
        let is_index = registry != "docker" && !entry.immutable;
        if !is_manifest && !is_index {
            continue;
        }
        if !seen_metadata.insert(&entry.digest) && !is_manifest {
            continue;
        }
        let Some(bytes) = metadata(&app, entry).await else {
            unavailable += 1;
            continue;
        };
        if registry == "docker" {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                docker_manifest(&app, entry, &value, &all, &mut manifests).await;
            } else {
                unavailable += 1;
            }
        } else if registry == "npm" {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if details.is_empty() {
                    for (label, key) in [
                        ("Description", "description"),
                        ("License", "license"),
                        ("Homepage", "homepage"),
                        ("Distribution tags", "dist-tags"),
                    ] {
                        info(&mut details, label, &text(value.get(key)));
                    }
                }
                npm_metadata(&app, entry, &value, &mut releases);
            } else {
                unavailable += 1;
            }
        } else {
            pypi_metadata(&app, entry, &bytes, &mut releases);
        }
    }
    // Include cached artifacts referenced by metadata even if their CDN paths do
    // not follow the usual registry naming convention.
    let referenced: BTreeSet<_> = releases
        .values()
        .map(|r| r.path.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    for entry in &all {
        if referenced.contains(decode(&entry.path).as_str())
            && !members.iter().any(|e| e.key == entry.key)
        {
            members.push(entry);
        }
    }
    html.push_str(&format!("<section class=\"grid\"><div class=\"card\">Cached objects<strong class=\"number\">{}</strong></div><div class=\"card\">Unique blob size<strong class=\"number\">{}</strong></div></section><dl>",members.len(),dashboard::size(unique_bytes(&members))));
    info(
        &mut html,
        "Last accessed (UTC)",
        &dashboard::timestamp(members.iter().map(|e| e.accessed_at).max().unwrap_or(0)),
    );
    html.push_str(&details);
    html.push_str("</dl>");
    if unavailable > 0 {
        html.push_str(&format!("<p>{unavailable} metadata objects are unavailable or unreadable. Cached file information is shown below.</p>"));
    }
    if registry == "docker" {
        html.push_str("<h2>Cached tags &amp; manifests</h2><div class=\"scroll\"><table><tr><th>Reference</th><th>Digest</th><th>Type / platforms</th><th>Layers</th></tr>");
        if manifests.is_empty() {
            html.push_str("<tr><td colspan=\"4\">No manifests currently cached.</td></tr>");
        }
        html.push_str(&manifests);
        html.push_str("</table></div>");
    } else {
        release_table(&mut html, &releases, &all, &registry);
    }
    html.push_str("<h2>Associated cached files</h2><p>Sizes count each digest once above. Times are UTC; cache policy describes the stored object.</p><div class=\"scroll\"><table><tr><th>Path</th><th>Size</th><th>SHA-256</th><th>Content type</th><th>Cached / last accessed</th><th>Policy</th></tr>");
    for entry in members {
        html.push_str(&format!("<tr><td class=\"path\">{}</td><td>{}</td><td class=\"path\"><code>{}</code></td><td>{}</td><td>{}<br>{}</td><td>{}</td></tr>",escape(&decode(&entry.path)),dashboard::size(entry.size.max(0) as u64),escape(&entry.digest),escape(&entry.content_type),dashboard::timestamp(entry.cached_at),dashboard::timestamp(entry.accessed_at),if entry.immutable {"Immutable"} else {"Refreshable metadata"}));
    }
    html.push_str("</table></div>");
    Ok(dashboard::page(&name, &registry, html))
}

async fn docker_manifest(
    app: &App,
    entry: &Entry,
    value: &Value,
    all: &[Entry],
    html: &mut String,
) {
    let reference = decode(&entry.path)
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_owned();
    let mut platforms = BTreeSet::new();
    if let Some(manifests) = value.get("manifests").and_then(Value::as_array) {
        for manifest in manifests {
            if let Some(p) = manifest.get("platform") {
                let mut platform = format!("{}/{}", text(p.get("os")), text(p.get("architecture")));
                if let Some(variant) = p.get("variant").and_then(Value::as_str) {
                    platform.push('/');
                    platform.push_str(variant);
                }
                platforms.insert(platform);
            }
        }
    }
    if let Some(digest) = value
        .pointer("/config/digest")
        .and_then(Value::as_str)
        .and_then(|s| s.strip_prefix("sha256:"))
    {
        if let Some(config) = all.iter().find(|e| e.digest == digest) {
            if let Some(bytes) = metadata(app, config).await {
                if let Ok(config) = serde_json::from_slice::<Value>(&bytes) {
                    platforms.insert(format!(
                        "{}/{}",
                        text(config.get("os")),
                        text(config.get("architecture"))
                    ));
                }
            }
        }
    }
    let kind = if value.get("manifests").is_some() {
        "Image index"
    } else {
        "Image manifest"
    };
    let layers = value
        .get("layers")
        .and_then(Value::as_array)
        .map(|v| v.len().to_string())
        .unwrap_or_else(|| "—".into());
    html.push_str(&format!("<tr><td class=\"path\">{}</td><td class=\"path\"><code>sha256:{}</code></td><td>{kind}<br>{}</td><td>{layers}</td></tr>",escape(&reference),escape(&entry.digest),escape(&platforms.into_iter().collect::<Vec<_>>().join(", "))));
}

fn release_table(
    html: &mut String,
    releases: &BTreeMap<String, Release>,
    all: &[Entry],
    registry: &str,
) {
    let cached: BTreeSet<_> = all
        .iter()
        .filter(|e| e.immutable)
        .map(|e| decode(&e.path))
        .collect();
    let mut rows: Vec<_> = releases.values().collect();
    rows.sort_by(|a, b| {
        cached
            .contains(&b.path)
            .cmp(&cached.contains(&a.path))
            .then_with(|| b.version.cmp(&a.version))
            .then_with(|| a.filename.cmp(&b.filename))
    });
    html.push_str(&format!("<h2>Versions from cached metadata</h2><p>{} records. Showing up to 200, cached artifacts first; version labels sort lexically. {} requirements are reported by the upstream.</p><div class=\"scroll\"><table><tr><th>Version / file</th><th>Artifact cached</th><th>Requirements</th><th>Integrity / notes</th></tr>",rows.len(),if registry == "npm" {"Engine"} else {"Python"}));
    for release in rows.into_iter().take(200) {
        html.push_str(&format!("<tr><td class=\"path\"><strong>{}</strong><br>{}</td><td>{}</td><td>{}</td><td class=\"path\">{}<br>{}</td></tr>",escape(&release.version),escape(&release.filename),if cached.contains(&release.path) {"Yes"} else {"No"},escape(&release.requires),escape(&release.integrity),escape(&release.notes)));
    }
    if releases.is_empty() {
        html.push_str("<tr><td colspan=\"4\">No package index metadata currently cached. See associated files below.</td></tr>");
    }
    html.push_str("</table></div>");
}
