use crate::{config::Config, Error};
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    http::{header, HeaderMap, Method, StatusCode},
    response::Response,
};
use futures_util::StreamExt;
use reqwest::Url;
use sha2::{Digest, Sha256};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    FromRow, SqlitePool,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{Mutex, OwnedRwLockReadGuard, RwLock},
};
use tokio_util::io::ReaderStream;

pub struct App {
    pub config: Config,
    pub db: SqlitePool,
    pub client: reqwest::Client,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub stale: AtomicU64,
    pub failures: AtomicU64,
    locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    gate: Arc<RwLock<()>>,
    _directory_lock: std::fs::File,
}

#[derive(Clone, FromRow, serde::Serialize)]
pub struct Entry {
    pub key: String,
    pub registry: String,
    pub path: String,
    pub digest: String,
    pub size: i64,
    pub content_type: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cached_at: i64,
    pub accessed_at: i64,
    pub immutable: bool,
}

pub struct Cached {
    pub entry: Entry,
    pub disposition: &'static str,
    _lease: OwnedRwLockReadGuard<()>,
}

pub struct Fetch {
    pub registry: &'static str,
    pub url: Url,
    pub accept: &'static str,
    pub immutable: bool,
    pub expected_sha256: Option<String>,
    pub metadata: bool,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

impl App {
    pub async fn new(config: Config) -> Result<Arc<Self>> {
        let root = Path::new(&config.cache.path);
        tokio::fs::create_dir_all(root.join("blobs/sha256")).await?;
        let directory_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join(".nexup.lock"))?;
        fs2::FileExt::try_lock_exclusive(&directory_lock)
            .context("data directory is already in use by another Nexup process")?;
        tokio::fs::create_dir_all(root.join("tmp")).await?;
        // One process owns a data directory. Remove incomplete downloads left by a crash.
        let mut temps = tokio::fs::read_dir(root.join("tmp")).await?;
        while let Some(item) = temps.next_entry().await? {
            if item.file_type().await?.is_file() {
                tokio::fs::remove_file(item.path()).await?;
            }
        }
        let db = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(root.join("metadata.sqlite"))
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .busy_timeout(Duration::from_secs(30)),
            )
            .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS entries (key TEXT PRIMARY KEY, registry TEXT NOT NULL, digest TEXT NOT NULL, size INTEGER NOT NULL, content_type TEXT NOT NULL, etag TEXT, last_modified TEXT, cached_at INTEGER NOT NULL, accessed_at INTEGER NOT NULL, immutable INTEGER NOT NULL)").execute(&db).await?;
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('entries')")
                .fetch_all(&db)
                .await?;
        if !columns.iter().any(|name| name == "path") {
            sqlx::query("ALTER TABLE entries ADD COLUMN path TEXT NOT NULL DEFAULT ''")
                .execute(&db)
                .await?;
        }
        sqlx::query("CREATE INDEX IF NOT EXISTS entries_digest ON entries(digest)")
            .execute(&db)
            .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY AUTOINCREMENT, time INTEGER NOT NULL, registry TEXT NOT NULL, path TEXT NOT NULL, outcome TEXT NOT NULL)").execute(&db).await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS upstream_status (registry TEXT PRIMARY KEY, checked_at INTEGER NOT NULL, healthy INTEGER NOT NULL, status TEXT NOT NULL)").execute(&db).await?;
        let env_proxy = |lower: &str, upper: &str| {
            std::env::var(lower)
                .ok()
                .or_else(|| std::env::var(upper).ok())
        };
        let no_proxy = env_proxy("no_proxy", "NO_PROXY").or_else(|| config.proxy.no_proxy.clone());
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .user_agent(concat!("nexup/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(config.server.request_timeout_seconds))
            .connect_timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::limited(10));
        if let Some(path) = std::env::var("SSL_CERT_FILE")
            .ok()
            .or_else(|| config.proxy.ssl_cert_file.clone())
        {
            let pem =
                std::fs::read(&path).with_context(|| format!("read SSL_CERT_FILE: {path}"))?;
            let certificates = reqwest::Certificate::from_pem_bundle(&pem)
                .context("parse SSL_CERT_FILE PEM certificate bundle")?;
            if certificates.is_empty() {
                bail!("SSL_CERT_FILE contains no PEM certificates");
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        if let Some(p) = env_proxy("http_proxy", "HTTP_PROXY")
            .or_else(|| config.proxy.http_proxy.clone())
            .filter(|v| !v.is_empty())
        {
            builder = builder.proxy(
                reqwest::Proxy::http(p)?
                    .no_proxy(no_proxy.as_deref().and_then(reqwest::NoProxy::from_string)),
            );
        }
        if let Some(p) = env_proxy("https_proxy", "HTTPS_PROXY")
            .or_else(|| config.proxy.https_proxy.clone())
            .filter(|v| !v.is_empty())
        {
            builder = builder.proxy(
                reqwest::Proxy::https(p)?
                    .no_proxy(no_proxy.as_deref().and_then(reqwest::NoProxy::from_string)),
            );
        }
        Ok(Arc::new(Self {
            config,
            db,
            client: builder.build()?,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            locks: Mutex::new(HashMap::new()),
            gate: Arc::new(RwLock::new(())),
            _directory_lock: directory_lock,
        }))
    }

    pub fn blob_path(&self, digest: &str) -> PathBuf {
        Path::new(&self.config.cache.path)
            .join("blobs/sha256")
            .join(&digest[..2])
            .join(digest)
    }

    async fn lookup(&self, key: &str) -> Result<Option<Entry>> {
        let entry = sqlx::query_as::<_, Entry>("SELECT * FROM entries WHERE key = ?")
            .bind(key)
            .fetch_optional(&self.db)
            .await?;
        if let Some(e) = &entry {
            if !tokio::fs::try_exists(self.blob_path(&e.digest)).await? {
                sqlx::query("DELETE FROM entries WHERE key = ?")
                    .bind(key)
                    .execute(&self.db)
                    .await?;
                return Ok(None);
            }
        }
        Ok(entry)
    }

    fn fresh(&self, e: &Entry) -> bool {
        e.immutable
            || now().saturating_sub(e.cached_at) < self.config.cache.metadata_ttl_seconds as i64
    }

    async fn touch(&self, key: &str) -> Result<()> {
        sqlx::query("UPDATE entries SET accessed_at = ? WHERE key = ?")
            .bind(now())
            .bind(key)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn event(&self, registry: &str, path: &str, outcome: &str) {
        let _ = sqlx::query("INSERT INTO events(time,registry,path,outcome) VALUES(?,?,?,?)")
            .bind(now())
            .bind(registry)
            .bind(path)
            .bind(outcome)
            .execute(&self.db)
            .await;
        let _ = sqlx::query("DELETE FROM events WHERE id <= (SELECT MAX(id) - 200 FROM events)")
            .execute(&self.db)
            .await;
    }

    async fn upstream_status(&self, registry: &str, healthy: bool, status: &str) {
        let _ = sqlx::query("INSERT INTO upstream_status VALUES(?,?,?,?) ON CONFLICT(registry) DO UPDATE SET checked_at=excluded.checked_at, healthy=excluded.healthy, status=excluded.status")
            .bind(registry).bind(now()).bind(healthy).bind(status).execute(&self.db).await;
    }

    pub async fn fetch(&self, spec: Fetch) -> Result<Cached, Error> {
        let lease = self.gate.clone().read_owned().await;
        let key = hex::encode(Sha256::digest(
            format!("{}\n{}\n{}", spec.registry, spec.url, spec.accept).as_bytes(),
        ));
        // Only hashes of URLs are kept as cache keys; signed URLs never appear in the dashboard.
        let lock = {
            let mut locks = self.locks.lock().await;
            if locks.len() > 4096 {
                locks.retain(|_, v| v.strong_count() > 0);
            }
            let lock = locks
                .get(&key)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| Arc::new(Mutex::new(())));
            locks.insert(key.clone(), Arc::downgrade(&lock));
            lock
        };
        let _guard = lock.lock().await;
        let old = self.lookup(&key).await?;
        if let (Some(entry), Some(expected)) = (&old, &spec.expected_sha256) {
            if !entry.digest.eq_ignore_ascii_case(expected) {
                return Err(Error(
                    StatusCode::BAD_GATEWAY,
                    "cached content does not match the requested SHA-256".into(),
                ));
            }
        }
        if let Some(e) = old.as_ref().filter(|e| self.fresh(e)) {
            self.touch(&key).await?;
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Cached {
                entry: e.clone(),
                disposition: "HIT",
                _lease: lease,
            });
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let result = self.download(&spec, &key, old.as_ref()).await;
        match result {
            Ok(entry) => {
                self.upstream_status(spec.registry, true, "reachable").await;
                self.event(spec.registry, spec.url.path(), "cached").await;
                Ok(Cached {
                    entry,
                    disposition: "MISS",
                    _lease: lease,
                })
            }
            Err(e) => {
                self.failures.fetch_add(1, Ordering::Relaxed);
                self.upstream_status(spec.registry, false, e.0.as_str())
                    .await;
                self.event(spec.registry, spec.url.path(), &format!("error {}", e.0))
                    .await;
                if self.config.cache.serve_stale_on_error
                    && (e.0.is_server_error() || e.0 == StatusCode::TOO_MANY_REQUESTS)
                {
                    if let Some(entry) = old {
                        self.touch(&key).await?;
                        self.stale.fetch_add(1, Ordering::Relaxed);
                        return Ok(Cached {
                            entry,
                            disposition: "STALE",
                            _lease: lease,
                        });
                    }
                }
                Err(e)
            }
        }
    }

    fn request(&self, spec: &Fetch, old: Option<&Entry>) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .get(spec.url.clone())
            .header(header::ACCEPT, spec.accept)
            .header(header::ACCEPT_ENCODING, "identity");
        if let Some(old) = old {
            if let Some(etag) = &old.etag {
                request = request.header(header::IF_NONE_MATCH, etag);
            } else if let Some(date) = &old.last_modified {
                request = request.header(header::IF_MODIFIED_SINCE, date);
            }
        }
        // Never send registry credentials to an external artifact host.
        let (upstream, token) = match spec.registry {
            "npm" => (&self.config.npm.upstream, &self.config.npm.token),
            "pypi" => (&self.config.pypi.upstream, &self.config.pypi.token),
            _ => return request,
        };
        if Url::parse(upstream).is_ok_and(|u| u.origin() == spec.url.origin()) {
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
        }
        request
    }

    async fn upstream(&self, spec: &Fetch, old: Option<&Entry>) -> Result<reqwest::Response> {
        let response = self.request(spec, old).send().await?;
        if spec.registry != "docker" || response.status() != StatusCode::UNAUTHORIZED {
            return Ok(response);
        }
        let challenge = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        let Some(params) = challenge
            .strip_prefix("Bearer ")
            .or_else(|| challenge.strip_prefix("bearer "))
        else {
            if let Some(username) = &self.config.docker.username {
                return Ok(self
                    .request(spec, old)
                    .basic_auth(username, self.config.docker.password.as_ref())
                    .send()
                    .await?);
            }
            return Ok(response);
        };
        let fields: HashMap<_, _> = params
            .split(',')
            .filter_map(|p| p.trim().split_once('='))
            .map(|(k, v)| (k, v.trim_matches('"')))
            .collect();
        let mut realm = Url::parse(fields.get("realm").context("missing token realm")?)?;
        let upstream = Url::parse(&self.config.docker.upstream)?;
        let docker_hub = upstream.host_str() == Some("registry-1.docker.io")
            && realm.host_str() == Some("auth.docker.io")
            && realm.scheme() == "https";
        if realm.origin() != upstream.origin() && !docker_hub {
            bail!("untrusted Docker authentication realm");
        }
        if !realm.username().is_empty() || realm.password().is_some() {
            bail!("invalid token realm");
        }
        for key in ["service", "scope"] {
            if let Some(value) = fields.get(key) {
                realm.query_pairs_mut().append_pair(key, value);
            }
        }
        let mut request = self.client.get(realm);
        if let Some(username) = &self.config.docker.username {
            request = request.basic_auth(username, self.config.docker.password.as_ref());
        }
        let token_response = request.send().await?;
        if !token_response.status().is_success() {
            return Ok(token_response);
        }
        let value: serde_json::Value = token_response.json().await?;
        let token = value
            .get("token")
            .or_else(|| value.get("access_token"))
            .and_then(|v| v.as_str())
            .context("missing Docker token")?;
        Ok(self.request(spec, old).bearer_auth(token).send().await?)
    }

    async fn download(&self, spec: &Fetch, key: &str, old: Option<&Entry>) -> Result<Entry, Error> {
        let response = self.upstream(spec, old).await?;
        if response.status() == StatusCode::NOT_MODIFIED {
            let mut entry = old.context("unexpected 304")?.clone();
            entry.cached_at = now();
            entry.accessed_at = now();
            self.store(&entry).await?;
            return Ok(entry);
        }
        if response.status() != StatusCode::OK {
            return Err(Error(
                response.status(),
                format!("{} upstream returned {}", spec.registry, response.status()),
            ));
        }
        let h = response.headers();
        let text = |name: &str| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let content_type =
            text("content-type").unwrap_or_else(|| "application/octet-stream".into());
        let etag = text("etag");
        let last_modified = text("last-modified");
        let header_digest = text("docker-content-digest")
            .and_then(|s| s.strip_prefix("sha256:").map(str::to_owned));
        let expected = spec.expected_sha256.as_ref().or(header_digest.as_ref());
        let tmp = Path::new(&self.config.cache.path)
            .join("tmp")
            .join(uuid::Uuid::new_v4().to_string());
        let mut cleanup = TempFile(tmp.clone());
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(anyhow::Error::from)?;
        let mut hash = Sha256::new();
        let mut size = 0u64;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(anyhow::Error::from)?;
            size += chunk.len() as u64;
            if (spec.metadata && size > self.config.cache.max_metadata_bytes)
                || (self.config.cache.max_size_bytes > 0 && size > self.config.cache.max_size_bytes)
            {
                return Err(Error(
                    StatusCode::BAD_GATEWAY,
                    "upstream object exceeds configured size limit".into(),
                ));
            }
            hash.update(&chunk);
            file.write_all(&chunk).await.map_err(anyhow::Error::from)?;
        }
        let digest = hex::encode(hash.finalize());
        if expected.is_some_and(|expected| !expected.eq_ignore_ascii_case(&digest)) {
            return Err(Error(
                StatusCode::BAD_GATEWAY,
                "upstream SHA-256 mismatch".into(),
            ));
        }
        file.sync_all().await.map_err(anyhow::Error::from)?;
        drop(file);
        let target = self.blob_path(&digest);
        tokio::fs::create_dir_all(target.parent().unwrap())
            .await
            .map_err(anyhow::Error::from)?;
        tokio::fs::rename(&tmp, &target)
            .await
            .map_err(anyhow::Error::from)?;
        cleanup.0 = PathBuf::new();
        let entry = Entry {
            key: key.into(),
            registry: spec.registry.into(),
            path: spec.url.path().into(),
            digest,
            size: size as i64,
            content_type,
            etag,
            last_modified,
            cached_at: now(),
            accessed_at: now(),
            immutable: spec.immutable,
        };
        self.store(&entry).await?;
        Ok(entry)
    }

    async fn store(&self, e: &Entry) -> Result<()> {
        sqlx::query("INSERT INTO entries(key,registry,digest,size,content_type,etag,last_modified,cached_at,accessed_at,immutable,path) VALUES(?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(key) DO UPDATE SET digest=excluded.digest,size=excluded.size,content_type=excluded.content_type,etag=excluded.etag,last_modified=excluded.last_modified,cached_at=excluded.cached_at,accessed_at=excluded.accessed_at,immutable=excluded.immutable,path=excluded.path")
            .bind(&e.key).bind(&e.registry).bind(&e.digest).bind(e.size).bind(&e.content_type).bind(&e.etag).bind(&e.last_modified).bind(e.cached_at).bind(e.accessed_at).bind(e.immutable).bind(&e.path).execute(&self.db).await?;
        Ok(())
    }

    pub async fn bytes(&self, cached: &Cached) -> Result<Vec<u8>> {
        if cached.entry.size as u64 > self.config.cache.max_metadata_bytes {
            bail!("metadata exceeds limit");
        }
        Ok(tokio::fs::read(self.blob_path(&cached.entry.digest)).await?)
    }

    pub async fn serve(
        &self,
        cached: Cached,
        method: Method,
        headers: HeaderMap,
        docker: bool,
    ) -> Result<Response, Error> {
        let e = &cached.entry;
        let etag = format!("\"sha256:{}\"", e.digest);
        let mut builder = Response::builder()
            .header(header::CONTENT_TYPE, &e.content_type)
            .header(header::ETAG, &etag)
            .header("x-nexup-cache", cached.disposition)
            .header(header::ACCEPT_RANGES, "bytes");
        if cached.disposition == "STALE" {
            builder = builder.header(header::WARNING, "110 nexup \"Response is stale\"");
        }
        if docker {
            builder = builder
                .header("docker-content-digest", format!("sha256:{}", e.digest))
                .header("docker-distribution-api-version", "registry/2.0");
        }
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(',').any(|v| v.trim() == etag || v.trim() == "*"))
        {
            return Ok(builder
                .status(StatusCode::NOT_MODIFIED)
                .body(Body::empty())
                .unwrap());
        }
        let size = e.size as u64;
        let range = if method != Method::HEAD
            && headers
                .get(header::IF_RANGE)
                .is_none_or(|v| v == etag.as_str())
        {
            headers
                .get(header::RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("bytes="))
        } else {
            None
        };
        let (start, length, status) = match range {
            Some(range) if !range.contains(',') => match parse_range(range, size) {
                Some((start, end)) => {
                    builder = builder
                        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
                    (start, end - start + 1, StatusCode::PARTIAL_CONTENT)
                }
                None => {
                    return Ok(builder
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                        .body(Body::empty())
                        .unwrap())
                }
            },
            _ => (0, size, StatusCode::OK),
        };
        let mut file = tokio::fs::File::open(self.blob_path(&e.digest))
            .await
            .map_err(anyhow::Error::from)?;
        if start > 0 {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(anyhow::Error::from)?;
        }
        // Open file descriptors remain readable after Linux GC unlinks a blob.
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from_stream(ReaderStream::new(file.take(length)))
        };
        Ok(builder
            .status(status)
            .header(header::CONTENT_LENGTH, length)
            .body(body)
            .unwrap())
    }

    pub async fn cleanup(&self) -> Result<()> {
        let _guard = self.gate.write().await;
        let mut total: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(size),0) FROM (SELECT digest,MAX(size) AS size FROM entries GROUP BY digest)").fetch_one(&self.db).await?;
        if self.config.cache.max_size_bytes > 0 {
            let candidates: Vec<(String, i64)> = sqlx::query_as(
                "SELECT digest,MAX(size) FROM entries GROUP BY digest ORDER BY MAX(accessed_at)",
            )
            .fetch_all(&self.db)
            .await?;
            for (digest, size) in candidates {
                if total <= self.config.cache.max_size_bytes as i64 {
                    break;
                }
                sqlx::query("DELETE FROM entries WHERE digest = ?")
                    .bind(&digest)
                    .execute(&self.db)
                    .await?;
                total -= size;
            }
        }
        // Also collect superseded metadata and crash-orphaned blobs.
        let referenced: std::collections::HashSet<String> =
            sqlx::query_scalar("SELECT DISTINCT digest FROM entries")
                .fetch_all(&self.db)
                .await?
                .into_iter()
                .collect();
        let mut dirs =
            tokio::fs::read_dir(Path::new(&self.config.cache.path).join("blobs/sha256")).await?;
        while let Some(dir) = dirs.next_entry().await? {
            if !dir.file_type().await?.is_dir() {
                continue;
            }
            let mut files = tokio::fs::read_dir(dir.path()).await?;
            while let Some(file) = files.next_entry().await? {
                if !referenced.contains(&file.file_name().to_string_lossy().to_string()) {
                    tokio::fs::remove_file(file.path()).await?;
                }
            }
        }
        Ok(())
    }
}

struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

fn parse_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let (start, end) = value.split_once('-')?;
    if size == 0 {
        return None;
    }
    if start.is_empty() {
        let suffix: u64 = end.parse().ok()?;
        return (suffix > 0).then_some((size.saturating_sub(suffix), size - 1));
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end && start < size).then_some((start, end))
}
