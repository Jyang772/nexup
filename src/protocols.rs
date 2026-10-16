use crate::{
    cache::{App, Fetch},
    response, Error,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const DOCKER_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";

fn bad(message: &str) -> Error {
    Error(StatusCode::BAD_REQUEST, message.into())
}
fn missing() -> Error {
    Error(
        StatusCode::NOT_FOUND,
        "registry or endpoint not found".into(),
    )
}

fn upstream(base: &str, segments: &[&str]) -> Result<Url, Error> {
    let mut url = Url::parse(base).map_err(anyhow::Error::from)?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| bad("invalid upstream URL"))?;
        path.pop_if_empty();
        for segment in segments {
            if segment.is_empty()
                || *segment == "."
                || *segment == ".."
                || segment.contains(['\\', '\0'])
            {
                return Err(bad("invalid path"));
            }
            path.push(segment);
        }
    }
    Ok(url)
}

pub async fn docker_ping(State(app): State<Arc<App>>) -> Result<Response, Error> {
    if !app.config.docker.enabled {
        return Err(missing());
    }
    let mut r = response(StatusCode::OK, "application/json", "{}");
    r.headers_mut().insert(
        "docker-distribution-api-version",
        "registry/2.0".parse().unwrap(),
    );
    Ok(r)
}

pub async fn docker(
    State(app): State<Arc<App>>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, Error> {
    if !app.config.docker.enabled {
        return Err(missing());
    }
    let (name, kind, reference) = if let Some((name, reference)) = path.rsplit_once("/manifests/") {
        (name, "manifests", reference)
    } else if let Some((name, reference)) = path.rsplit_once("/blobs/") {
        (name, "blobs", reference)
    } else {
        return Err(missing());
    };
    if name.len() > 255
        || !name.split('/').all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        })
    {
        return Err(bad("invalid repository name"));
    }
    let digest = reference.strip_prefix("sha256:");
    if let Some(digest) = digest {
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(bad("invalid SHA-256 digest"));
        }
    } else if kind == "blobs"
        || reference.is_empty()
        || reference.len() > 128
        || !reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(bad("invalid image reference"));
    }
    let name = if !name.contains('/') {
        format!("library/{name}")
    } else {
        name.to_owned()
    };
    let mut segments = vec!["v2"];
    segments.extend(name.split('/'));
    segments.extend([kind, reference]);
    let url = upstream(&app.config.docker.upstream, &segments)?;
    let cached = app
        .fetch(Fetch {
            registry: "docker",
            url,
            accept: if kind == "manifests" {
                DOCKER_ACCEPT
            } else {
                "application/octet-stream"
            },
            immutable: digest.is_some(),
            expected_sha256: digest.map(str::to_owned),
            metadata: kind == "manifests",
        })
        .await?;
    app.serve(cached, method, headers, true).await
}

fn artifact_url(app: &App, registry: &str, url: &Url) -> String {
    let prefix = if registry == "npm" {
        "/npm/-/artifact/"
    } else {
        "/pypi/files/"
    };
    let mut rewritten = format!(
        "{}{prefix}{}",
        app.config.server.public_url,
        URL_SAFE_NO_PAD.encode(url.as_str())
    );
    // pip identifies wheel names and versions from the URL's final path segment.
    if registry == "pypi" {
        rewritten.push('/');
        rewritten.push_str(url.path().rsplit('/').next().unwrap_or("artifact"));
    }
    rewritten
}

fn validate_artifact(app: &App, registry: &str, url: &Url) -> Result<(), Error> {
    let (base, hosts) = if registry == "npm" {
        (&app.config.npm.upstream, &app.config.npm.artifact_hosts)
    } else {
        (&app.config.pypi.upstream, &app.config.pypi.artifact_hosts)
    };
    let configured = Url::parse(base).map_err(anyhow::Error::from)?;
    let allowed = url.origin() == configured.origin()
        || (url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && hosts.iter().any(|h| Some(h.as_str()) == url.host_str()));
    if !allowed || !url.username().is_empty() || url.password().is_some() {
        return Err(bad("artifact host is not configured for this registry"));
    }
    Ok(())
}

async fn artifact(
    app: &App,
    registry: &'static str,
    mut url: Url,
    metadata: bool,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, Error> {
    validate_artifact(app, registry, &url)?;
    let digest = url
        .fragment()
        .and_then(|f| f.strip_prefix("sha256="))
        .map(str::to_owned);
    url.set_fragment(None);
    let cached = app
        .fetch(Fetch {
            registry,
            url,
            accept: "application/octet-stream",
            immutable: true,
            expected_sha256: digest,
            metadata,
        })
        .await?;
    app.serve(cached, method, headers, false).await
}

fn decode_url(value: &str) -> Result<Url, Error> {
    if value.len() > 16384 {
        return Err(bad("artifact URL is too long"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| bad("invalid artifact URL"))?;
    Url::parse(std::str::from_utf8(&bytes).map_err(|_| bad("invalid artifact URL"))?)
        .map_err(|_| bad("invalid artifact URL"))
}

pub async fn npm(
    State(app): State<Arc<App>>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, Error> {
    if !app.config.npm.enabled {
        return Err(missing());
    }
    if let Some(encoded) = path.strip_prefix("-/artifact/") {
        return artifact(&app, "npm", decode_url(encoded)?, false, method, headers).await;
    }
    if path.starts_with('-') || path.len() > 1024 {
        return Err(missing());
    }
    let parts: Vec<_> = path.split('/').collect();
    let url = upstream(&app.config.npm.upstream, &parts)?;
    // Support conventional tarball paths as well as rewritten metadata URLs.
    if path.contains("/-/") && path.ends_with(".tgz") {
        return artifact(&app, "npm", url, false, method, headers).await;
    }
    let accept = if headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/vnd.npm.install-v1+json"))
    {
        "application/vnd.npm.install-v1+json"
    } else {
        "application/json"
    };
    let cached = app
        .fetch(Fetch {
            registry: "npm",
            url: url.clone(),
            accept,
            immutable: false,
            expected_sha256: None,
            metadata: true,
        })
        .await?;
    let mut value: serde_json::Value =
        serde_json::from_slice(&app.bytes(&cached).await?).map_err(anyhow::Error::from)?;
    rewrite_npm(&app, &url, &mut value)?;
    metadata_response(
        serde_json::to_vec(&value).map_err(anyhow::Error::from)?,
        accept,
        cached.disposition,
        method,
        headers,
    )
}

fn rewrite_npm(app: &App, base: &Url, value: &mut serde_json::Value) -> Result<(), Error> {
    if let Some(dist) = value.get_mut("dist").and_then(|v| v.as_object_mut()) {
        if let Some(tarball) = dist.get("tarball").and_then(|v| v.as_str()) {
            let url = base.join(tarball).map_err(anyhow::Error::from)?;
            validate_artifact(app, "npm", &url)?;
            dist.insert("tarball".into(), artifact_url(app, "npm", &url).into());
        }
    }
    if let Some(versions) = value.get_mut("versions").and_then(|v| v.as_object_mut()) {
        for version in versions.values_mut() {
            rewrite_npm(app, base, version)?;
        }
    }
    Ok(())
}

pub async fn pypi_root(State(app): State<Arc<App>>) -> Result<Response, Error> {
    if !app.config.pypi.enabled {
        return Err(missing());
    }
    // pip requests projects directly; avoid mirroring PyPI's enormous project listing.
    Ok(response(StatusCode::OK, "text/html; charset=utf-8", "<!doctype html><html><body><p>Nexup PyPI proxy. Request /pypi/simple/&lt;project&gt;/.</p></body></html>"))
}

pub async fn pypi(
    State(app): State<Arc<App>>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, Error> {
    if !app.config.pypi.enabled {
        return Err(missing());
    }
    let name = path.trim_end_matches('/');
    if name.is_empty()
        || name.len() > 200
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(bad("invalid Python project name"));
    }
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
    let mut url = upstream(&app.config.pypi.upstream, &[&normalized])?;
    url.path_segments_mut().unwrap().push("");
    let json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/vnd.pypi.simple.v1+json"));
    let accept = if json {
        "application/vnd.pypi.simple.v1+json"
    } else {
        "application/vnd.pypi.simple.v1+html, text/html;q=0.9"
    };
    let cached = app
        .fetch(Fetch {
            registry: "pypi",
            url: url.clone(),
            accept,
            immutable: false,
            expected_sha256: None,
            metadata: true,
        })
        .await?;
    let bytes = app.bytes(&cached).await?;
    let is_json = cached.entry.content_type.contains("json");
    let (body, content_type) = if is_json {
        let mut value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(anyhow::Error::from)?;
        if let Some(files) = value.get_mut("files").and_then(|v| v.as_array_mut()) {
            for file in files {
                if let Some(href) = file.get("url").and_then(|v| v.as_str()) {
                    let mut target = url.join(href).map_err(anyhow::Error::from)?;
                    if let Some(hash) = file.pointer("/hashes/sha256").and_then(|v| v.as_str()) {
                        target.set_fragment(Some(&format!("sha256={hash}")));
                    }
                    validate_artifact(&app, "pypi", &target)?;
                    file["url"] = artifact_url(&app, "pypi", &target).into();
                }
            }
        }
        (
            serde_json::to_vec(&value).map_err(anyhow::Error::from)?,
            "application/vnd.pypi.simple.v1+json",
        )
    } else {
        (
            rewrite_pypi_html(&app, &url, &bytes)?.into_bytes(),
            "application/vnd.pypi.simple.v1+html; charset=utf-8",
        )
    };
    metadata_response(body, content_type, cached.disposition, method, headers)
}

fn rewrite_pypi_html(app: &App, base: &Url, bytes: &[u8]) -> Result<String, Error> {
    let text = std::str::from_utf8(bytes).map_err(anyhow::Error::from)?;
    let document = scraper::Html::parse_document(text);
    let selector = scraper::Selector::parse("a[href]").unwrap();
    let mut html = String::from("<!doctype html><html><head><meta name=\"pypi:repository-version\" content=\"1.0\"></head><body>\n");
    for link in document.select(&selector) {
        let mut target = base
            .join(link.value().attr("href").unwrap())
            .map_err(anyhow::Error::from)?;
        validate_artifact(app, "pypi", &target)?;
        let fragment = target.fragment().map(str::to_owned);
        // Keep the hash visible to pip as well as encoded for server verification.
        let rewritten = artifact_url(app, "pypi", &target);
        target.set_fragment(None);
        html.push_str(&format!(
            "<a href=\"{}{}\"",
            escape(&rewritten),
            fragment
                .map(|f| format!("#{}", escape(&f)))
                .unwrap_or_default()
        ));
        for name in [
            "data-requires-python",
            "data-yanked",
            "data-dist-info-metadata",
            "data-core-metadata",
            "data-provenance",
        ] {
            if let Some(value) = link.value().attr(name) {
                html.push_str(&format!(" {name}=\"{}\"", escape(value)));
            }
        }
        html.push_str(&format!(
            ">{}</a>\n",
            escape(&link.text().collect::<String>())
        ));
    }
    html.push_str("</body></html>");
    Ok(html)
}

pub async fn pypi_file(
    State(app): State<Arc<App>>,
    Path(path): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, Error> {
    if !app.config.pypi.enabled {
        return Err(missing());
    }
    let (encoded, metadata) = path
        .strip_suffix(".metadata")
        .map(|p| (p, true))
        .unwrap_or((&path, false));
    let mut url = decode_url(encoded.split('/').next().unwrap_or(encoded))?;
    if metadata {
        url.set_fragment(None);
        url.set_path(&format!("{}.metadata", url.path()));
    }
    artifact(&app, "pypi", url, metadata, method, headers).await
}

fn metadata_response(
    bytes: Vec<u8>,
    content_type: &str,
    disposition: &str,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let etag = format!("\"{}\"", hex::encode(Sha256::digest(&bytes)));
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ETAG, &etag)
        .header(header::VARY, "Accept")
        .header("x-nexup-cache", disposition);
    if disposition == "STALE" {
        builder = builder.header(header::WARNING, "110 nexup \"Response is stale\"");
    }
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|s| s.trim() == etag || s.trim() == "*"))
    {
        return Ok(builder
            .status(StatusCode::NOT_MODIFIED)
            .body(axum::body::Body::empty())
            .unwrap());
    }
    builder = builder.header(header::CONTENT_LENGTH, bytes.len());
    Ok(builder
        .body(if method == Method::HEAD {
            axum::body::Body::empty()
        } else {
            bytes.into()
        })
        .unwrap())
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
