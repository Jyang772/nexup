use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use nexup::{config::Config, App};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Default)]
struct Upstream {
    count: AtomicUsize,
    mode: AtomicUsize,
}

async fn mock(
    State(state): State<Arc<Upstream>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.count.fetch_add(1, Ordering::SeqCst);
    match state.mode.load(Ordering::SeqCst) {
        1 => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        2 => return StatusCode::NOT_FOUND.into_response(),
        _ => {}
    }
    let host = headers.get("host").unwrap().to_str().unwrap();
    let base = format!("http://{host}");
    if path == "token" {
        return Json(serde_json::json!({"token":"test-token"})).into_response();
    }
    if path.starts_with("v2/") {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer test-token") {
            return (StatusCode::UNAUTHORIZED, [("www-authenticate", format!("Bearer realm=\"{base}/token\",service=\"test\",scope=\"repository:library/demo:pull\""))]).into_response();
        }
        if path.contains("/blobs/") {
            return (
                [("content-type", "application/octet-stream")],
                "layer bytes",
            )
                .into_response();
        }
        if path.ends_with("/manifests/multi") {
            return ([("content-type","application/vnd.oci.image.index.v1+json")],Json(serde_json::json!({"schemaVersion":2,"manifests":[{"digest":format!("sha256:{}","a".repeat(64)),"platform":{"os":"linux","architecture":"amd64"}},{"digest":format!("sha256:{}","b".repeat(64)),"platform":{"os":"linux","architecture":"arm64","variant":"v8"}}]}))).into_response();
        }
        return (
            [("content-type", "application/vnd.oci.image.manifest.v1+json")],
            "{\"schemaVersion\":2}",
        )
            .into_response();
    }
    if path.ends_with(".metadata") {
        return "Metadata-Version: 2.1\nName: demo\nVersion: 1.0\n".into_response();
    }
    if path.starts_with("files/") {
        return "wheel bytes".into_response();
    }
    if path == "simple/demo/" {
        let hash = hex::encode(Sha256::digest(b"wheel bytes"));
        if headers
            .get("accept")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("json")
        {
            return ([("content-type","application/vnd.pypi.simple.v1+json")], Json(serde_json::json!({"meta":{"api-version":"1.0"},"name":"demo","files":[{"filename":"demo-1.0-py3-none-any.whl","url":format!("{base}/files/demo-1.0-py3-none-any.whl"),"hashes":{"sha256":hash},"requires-python":">=3.8","core-metadata":true,"yanked":false}]}))).into_response();
        }
        return ([("content-type","text/html")],format!("<a href=\"{base}/files/demo-1.0-py3-none-any.whl#sha256={hash}\" data-requires-python=\"&gt;=3.8\" data-core-metadata=\"true\" data-yanked=\"reason\">demo-1.0-py3-none-any.whl</a>")).into_response();
    }
    if path.ends_with(".tgz") {
        return "tarball bytes".into_response();
    }
    if headers
        .get("if-none-match")
        .is_some_and(|v| v == "\"upstream-v1\"")
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }
    ([("etag","\"upstream-v1\"")],Json(serde_json::json!({"name":"demo","description":"A <script>alert('test')</script> package","license":"MIT","dist-tags":{"latest":"1.0.0"},"versions":{"1.0.0":{"name":"demo","version":"1.0.0","dist":{"tarball":format!("{base}/demo/-/demo-1.0.0.tgz"),"integrity":"sha512-test"}}}}))).into_response()
}

struct Harness {
    _dir: tempfile::TempDir,
    app: Arc<App>,
    base: String,
    upstream: Arc<Upstream>,
    client: reqwest::Client,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
impl Harness {
    async fn new(configure: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Upstream::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_base = format!("http://{}", listener.local_addr().unwrap());
        let upstream_router = Router::new()
            .route("/{*path}", get(mock))
            .with_state(state.clone());
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream_router).await.unwrap();
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut config = Config::default();
        config.cache.path = dir.path().to_str().unwrap().into();
        config.server.public_url = base.clone();
        config.proxy.no_proxy = Some("*".into());
        config.docker.upstream = upstream_base.clone();
        config.npm.upstream = upstream_base.clone();
        config.pypi.upstream = format!("{upstream_base}/simple");
        configure(&mut config);
        let app = App::new(config).await.unwrap();
        let router = nexup::router(app.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            _dir: dir,
            app,
            base,
            upstream: state,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            tasks: vec![upstream_task, server],
        }
    }
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.client.get(format!("{}{path}", self.base))
    }
}

#[tokio::test]
async fn docker_token_flow_integrity_ranges_and_offline_cache() {
    let h = Harness::new(|_| {}).await;
    let digest = hex::encode(Sha256::digest(b"layer bytes"));
    let path = format!("/v2/library/demo/blobs/sha256:{digest}");
    let r = h.get(&path).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()["docker-content-digest"],
        format!("sha256:{digest}")
    );
    assert_eq!(r.text().await.unwrap(), "layer bytes");
    h.upstream.mode.store(1, Ordering::SeqCst);
    let r = h
        .get(&path)
        .header("Range", "bytes=2-5")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["x-nexup-cache"], "HIT");
    assert_eq!(r.text().await.unwrap(), "yer ");
    let r = h
        .client
        .head(format!("{}{path}", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["content-length"], "11");
    assert!(r.bytes().await.unwrap().is_empty());
    assert_eq!(
        h.get(&path)
            .header("Range", "bytes=50-60")
            .send()
            .await
            .unwrap()
            .status(),
        416
    );
}

#[tokio::test]
async fn checksum_mismatch_never_publishes_blob() {
    let h = Harness::new(|_| {}).await;
    let r = h
        .get(&format!("/v2/demo/blobs/sha256:{}", "0".repeat(64)))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 502);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entries")
        .fetch_one(&h.app.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        std::fs::read_dir(h._dir.path().join("tmp"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn npm_rewrites_tarballs_and_coalesces_concurrent_misses() {
    let h = Harness::new(|_| {}).await;
    let requests = (0..12).map(|_| h.get("/npm/@scope/demo").send());
    for r in futures_util::future::join_all(requests).await {
        assert_eq!(r.unwrap().status(), 200);
    }
    assert_eq!(h.upstream.count.load(Ordering::SeqCst), 1);
    let metadata: serde_json::Value = h
        .get("/npm/@scope/demo")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = metadata["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert!(url.starts_with(&format!("{}/npm/-/artifact/", h.base)));
    assert_eq!(
        h.client
            .get(url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "tarball bytes"
    );
    h.upstream.mode.store(1, Ordering::SeqCst);
    let r = h.client.get(url).send().await.unwrap();
    assert_eq!(r.headers()["x-nexup-cache"], "HIT");
}

#[tokio::test]
async fn stale_metadata_on_503_but_not_404_and_conditional_refresh() {
    let h = Harness::new(|c| c.cache.metadata_ttl_seconds = 0).await;
    let first = h.get("/npm/demo").send().await.unwrap();
    let etag = first.headers()["etag"].clone();
    assert_eq!(first.status(), 200);
    // Upstream responds 304, while rewritten downstream bytes have their own ETag.
    assert_eq!(
        h.get("/npm/demo")
            .header("If-None-Match", etag)
            .send()
            .await
            .unwrap()
            .status(),
        304
    );
    h.upstream.mode.store(1, Ordering::SeqCst);
    let stale = h.get("/npm/demo").send().await.unwrap();
    assert_eq!(stale.status(), 200);
    assert_eq!(stale.headers()["x-nexup-cache"], "STALE");
    h.upstream.mode.store(2, Ordering::SeqCst);
    assert_eq!(h.get("/npm/demo").send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn pypi_json_html_and_metadata_downloads() {
    let h = Harness::new(|_| {}).await;
    let page = h
        .get("/pypi/simple/DEMO/")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("data-requires-python=\"&gt;=3.8\""));
    assert!(page.contains("data-yanked=\"reason\""));
    let metadata: serde_json::Value = h
        .get("/pypi/simple/demo/")
        .header("Accept", "application/vnd.pypi.simple.v1+json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = metadata["files"][0]["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{}/pypi/files/", h.base)));
    assert_eq!(
        h.client
            .get(url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "wheel bytes"
    );
    let r = h
        .client
        .get(format!("{url}.metadata"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Metadata-Version"));
    h.upstream.mode.store(1, Ordering::SeqCst);
    assert_eq!(
        h.client.get(url).send().await.unwrap().headers()["x-nexup-cache"],
        "HIT"
    );
}

#[tokio::test]
async fn anonymous_client_and_dashboard_auth_are_separate() {
    let h = Harness::new(|c| {
        c.auth.allow_anonymous_pull = false;
        c.auth.password = Some("pull-secret".into());
        c.auth.dashboard_public = false;
        c.auth.admin_token = Some("admin-secret".into());
    })
    .await;
    assert_eq!(h.get("/health").send().await.unwrap().status(), 200);
    assert_eq!(h.get("/v2/").send().await.unwrap().status(), 401);
    assert_eq!(
        h.get("/v2/")
            .basic_auth("nexup", Some("pull-secret"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.get("/npm/demo")
            .bearer_auth("pull-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.get("/")
            .bearer_auth("pull-secret")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        h.get("/")
            .basic_auth("admin", Some("admin-secret"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.get("/api/status")
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn reject_unconfigured_artifact_hosts_and_disabled_registries() {
    use base64::Engine;
    let h = Harness::new(|c| c.docker.enabled = false).await;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode("http://169.254.169.254/latest/meta-data/");
    assert_eq!(
        h.get(&format!("/pypi/files/{encoded}"))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(h.get("/v2/").send().await.unwrap().status(), 404);
    assert_eq!(h.upstream.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn size_limit_cleanup_and_exclusive_directory_ownership() {
    let h = Harness::new(|c| c.cache.max_size_bytes = 1).await;
    assert_eq!(h.get("/npm/demo").send().await.unwrap().status(), 502);
    assert!(App::new(h.app.config.clone()).await.is_err());
    let h = Harness::new(|_| {}).await;
    assert_eq!(h.get("/npm/demo").send().await.unwrap().status(), 200);
    let orphan = h.app.blob_path(&"f".repeat(64));
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"orphan").unwrap();
    h.app.cleanup().await.unwrap();
    assert!(!orphan.exists());
    assert_eq!(
        h.get("/npm/demo").send().await.unwrap().headers()["x-nexup-cache"],
        "HIT"
    );
}

#[tokio::test]
async fn cleanup_evicts_shared_blobs_to_storage_target() {
    let h = Harness::new(|c| c.cache.max_size_bytes = 20).await;
    let digest = hex::encode(Sha256::digest(b"layer bytes"));
    assert_eq!(
        h.get(&format!("/v2/demo/blobs/sha256:{digest}"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.get("/npm/demo/-/demo.tgz").send().await.unwrap().status(),
        200
    );
    let before: i64 = sqlx::query_scalar("SELECT SUM(size) FROM entries")
        .fetch_one(&h.app.db)
        .await
        .unwrap();
    assert!(before > 20);
    h.app.cleanup().await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT SUM(size) FROM entries")
        .fetch_one(&h.app.db)
        .await
        .unwrap();
    assert!(after <= 20);
}

#[tokio::test]
async fn cached_artifact_still_checks_new_hash_claims() {
    use base64::Engine;
    let h = Harness::new(|_| {}).await;
    let origin = h.app.config.npm.upstream.clone();
    let encode = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
    let path = format!(
        "/pypi/files/{}",
        encode(&format!("{origin}/files/demo.whl"))
    );
    assert_eq!(h.get(&path).send().await.unwrap().status(), 200);
    let wrong = format!("{origin}/files/demo.whl#sha256={}", "0".repeat(64));
    assert_eq!(
        h.get(&format!("/pypi/files/{}", encode(&wrong)))
            .send()
            .await
            .unwrap()
            .status(),
        502
    );
}

#[tokio::test]
async fn registry_pages_group_scoped_packages_and_display_metadata_without_fetching() {
    let h = Harness::new(|_| {}).await;
    let metadata: serde_json::Value = h
        .get("/npm/@scope/demo")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let tarball = metadata["versions"]["1.0.0"]["dist"]["tarball"]
        .as_str()
        .unwrap();
    assert_eq!(h.client.get(tarball).send().await.unwrap().status(), 200);
    let count = h.upstream.count.load(Ordering::SeqCst);
    h.upstream.mode.store(1, Ordering::SeqCst);
    let index = h
        .get("/ui/registries/npm?q=%40scope")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let document = scraper::Html::parse_document(&index);
    let selector = scraper::Selector::parse("a[href]").unwrap();
    let href = document
        .select(&selector)
        .find(|a| a.text().collect::<String>() == "@scope/demo")
        .unwrap()
        .value()
        .attr("href")
        .unwrap();
    let detail = h.get(href).send().await.unwrap();
    assert_eq!(detail.status(), 200);
    assert_eq!(detail.headers()["cache-control"], "no-store");
    let detail = detail.text().await.unwrap();
    assert!(detail.contains("1.0.0"));
    assert!(detail.contains("Distribution tags"));
    assert!(detail.contains("sha512-test"));
    assert!(detail.contains("demo-1.0.0.tgz"));
    assert!(detail.contains("<td>Yes</td>"));
    assert!(detail.contains("&lt;script&gt;"));
    assert!(!detail.contains("<script>"));
    assert_eq!(h.upstream.count.load(Ordering::SeqCst), count);
    assert_eq!(
        h.get("/ui/registries/npm/not-cached")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        h.get("/ui/registries/unknown")
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn docker_and_pypi_detail_pages_show_protocol_specific_information() {
    let h = Harness::new(|_| {}).await;
    assert_eq!(
        h.get("/v2/demo/manifests/multi")
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let digest = hex::encode(Sha256::digest(b"layer bytes"));
    assert_eq!(
        h.get(&format!("/v2/demo/blobs/sha256:{digest}"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let page = h
        .get("/ui/registries/docker/library/demo")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("linux/amd64"));
    assert!(page.contains("linux/arm64/v8"));
    assert!(page.contains("Image index"));
    assert!(page.contains(&digest));
    let metadata: serde_json::Value = h
        .get("/pypi/simple/demo/")
        .header("Accept", "application/vnd.pypi.simple.v1+json")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let url = metadata["files"][0]["url"].as_str().unwrap();
    assert_eq!(h.client.get(url).send().await.unwrap().status(), 200);
    let page = h
        .get("/ui/registries/pypi/demo")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("demo-1.0-py3-none-any.whl"));
    assert!(page.contains("&gt;=3.8"));
    assert!(page.contains("<td>Yes</td>"));
    assert!(page.contains(&hex::encode(Sha256::digest(b"wheel bytes"))));
    let list = h
        .get("/ui/registries/pypi")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(list.contains("1 cached images/packages"));
}

#[tokio::test]
async fn registry_pages_require_dashboard_credentials_even_when_pulls_are_anonymous() {
    let h = Harness::new(|c| {
        c.auth.dashboard_public = false;
        c.auth.admin_token = Some("admin".into());
        c.auth.password = Some("pull".into());
    })
    .await;
    assert_eq!(h.get("/npm/demo").send().await.unwrap().status(), 200);
    for path in ["/ui/registries/npm", "/ui/registries/npm/demo"] {
        assert_eq!(h.get(path).send().await.unwrap().status(), 401);
        assert_eq!(
            h.get(path)
                .bearer_auth("pull")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            h.get(path)
                .bearer_auth("admin")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
}

#[tokio::test]
async fn registry_lists_search_paginate_and_tolerate_evicted_metadata() {
    let h = Harness::new(|_| {}).await;
    for index in 0..52 {
        assert_eq!(
            h.get(&format!("/npm/package-{index:02}"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let first = h
        .get("/ui/registries/npm")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(first.contains("Page 1 of 2"));
    assert!(!first.contains(">package-51</a>"));
    let second = h
        .get("/ui/registries/npm?page=1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(second.contains(">package-51</a>"));
    let search = h
        .get("/ui/registries/npm?q=package-51")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(search.contains("1 matching"));
    let digest: String = sqlx::query_scalar("SELECT digest FROM entries LIMIT 1")
        .fetch_one(&h.app.db)
        .await
        .unwrap();
    tokio::fs::remove_file(h.app.blob_path(&digest))
        .await
        .unwrap();
    let detail = h
        .get("/ui/registries/npm/package-00")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(detail.contains("unavailable or unreadable"));
    assert!(detail.contains("Associated cached files"));
}
