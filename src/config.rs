use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{env, path::Path};

#[derive(Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub auth: Auth,
    pub cache: Cache,
    pub proxy: Proxy,
    pub docker: Registry,
    pub npm: Npm,
    pub pypi: Pypi,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub listen: String,
    pub public_url: String,
    pub request_timeout_seconds: u64,
}
impl Default for Server {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".into(),
            public_url: "http://localhost:8080".into(),
            request_timeout_seconds: 300,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Auth {
    pub allow_anonymous_pull: bool,
    pub username: String,
    pub password: Option<String>,
    pub admin_token: Option<String>,
    pub dashboard_public: bool,
}
impl Default for Auth {
    fn default() -> Self {
        Self {
            allow_anonymous_pull: true,
            username: "nexup".into(),
            password: None,
            admin_token: None,
            dashboard_public: true,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cache {
    pub path: String,
    pub metadata_ttl_seconds: u64,
    pub serve_stale_on_error: bool,
    pub max_size_bytes: u64,
    pub cleanup_interval_seconds: u64,
    pub max_metadata_bytes: u64,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            path: "./data".into(),
            metadata_ttl_seconds: 300,
            serve_stale_on_error: true,
            max_size_bytes: 50 * 1024 * 1024 * 1024,
            cleanup_interval_seconds: 300,
            max_metadata_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Proxy {
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub ssl_cert_file: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Registry {
    pub enabled: bool,
    pub upstream: String,
    pub username: Option<String>,
    pub password: Option<String>,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            enabled: true,
            upstream: "https://registry-1.docker.io".into(),
            username: None,
            password: None,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Npm {
    pub enabled: bool,
    pub upstream: String,
    pub token: Option<String>,
    pub artifact_hosts: Vec<String>,
}
impl Default for Npm {
    fn default() -> Self {
        Self {
            enabled: true,
            upstream: "https://registry.npmjs.org".into(),
            token: None,
            artifact_hosts: vec![],
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pypi {
    pub enabled: bool,
    pub upstream: String,
    pub token: Option<String>,
    pub artifact_hosts: Vec<String>,
}
impl Default for Pypi {
    fn default() -> Self {
        Self {
            enabled: true,
            upstream: "https://pypi.org/simple".into(),
            token: None,
            artifact_hosts: vec!["files.pythonhosted.org".into()],
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut c: Self = match path {
            Some(path) => {
                toml::from_str(&std::fs::read_to_string(path).context("read configuration")?)
                    .context("parse configuration")?
            }
            None => Self::default(),
        };
        macro_rules! string_env {
            ($name:literal, $field:expr) => {
                if let Ok(v) = env::var($name) {
                    $field = v;
                }
            };
        }
        macro_rules! optional_env {
            ($name:literal, $field:expr) => {
                if let Ok(v) = env::var($name) {
                    $field = Some(v);
                }
            };
        }
        macro_rules! parse_env {
            ($name:literal, $field:expr) => {
                if let Ok(v) = env::var($name) {
                    $field = v.parse().with_context(|| concat!("invalid ", $name))?;
                }
            };
        }
        string_env!("NEXUP_LISTEN", c.server.listen);
        string_env!("NEXUP_PUBLIC_URL", c.server.public_url);
        string_env!("NEXUP_DATA_DIR", c.cache.path);
        string_env!("NEXUP_AUTH_USERNAME", c.auth.username);
        optional_env!("NEXUP_AUTH_PASSWORD", c.auth.password);
        optional_env!("NEXUP_ADMIN_TOKEN", c.auth.admin_token);
        parse_env!("NEXUP_ALLOW_ANONYMOUS_PULL", c.auth.allow_anonymous_pull);
        parse_env!("NEXUP_DASHBOARD_PUBLIC", c.auth.dashboard_public);
        parse_env!("NEXUP_METADATA_TTL_SECONDS", c.cache.metadata_ttl_seconds);
        parse_env!("NEXUP_SERVE_STALE_ON_ERROR", c.cache.serve_stale_on_error);
        parse_env!("NEXUP_MAX_SIZE_BYTES", c.cache.max_size_bytes);
        parse_env!(
            "NEXUP_CLEANUP_INTERVAL_SECONDS",
            c.cache.cleanup_interval_seconds
        );
        parse_env!(
            "NEXUP_REQUEST_TIMEOUT_SECONDS",
            c.server.request_timeout_seconds
        );
        parse_env!("NEXUP_DOCKER_ENABLED", c.docker.enabled);
        parse_env!("NEXUP_NPM_ENABLED", c.npm.enabled);
        parse_env!("NEXUP_PYPI_ENABLED", c.pypi.enabled);
        string_env!("NEXUP_DOCKER_UPSTREAM", c.docker.upstream);
        string_env!("NEXUP_NPM_UPSTREAM", c.npm.upstream);
        string_env!("NEXUP_PYPI_UPSTREAM", c.pypi.upstream);
        optional_env!("NEXUP_DOCKER_USERNAME", c.docker.username);
        optional_env!("NEXUP_DOCKER_PASSWORD", c.docker.password);
        optional_env!("NEXUP_NPM_TOKEN", c.npm.token);
        optional_env!("NEXUP_PYPI_TOKEN", c.pypi.token);
        c.server.public_url = c.server.public_url.trim_end_matches('/').to_owned();
        for value in [
            &c.server.public_url,
            &c.docker.upstream,
            &c.npm.upstream,
            &c.pypi.upstream,
        ] {
            let u = url::Url::parse(value).context("invalid configured URL")?;
            if !matches!(u.scheme(), "http" | "https")
                || u.host_str().is_none()
                || !u.username().is_empty()
                || u.password().is_some()
                || u.query().is_some()
                || u.fragment().is_some()
            {
                bail!("configured URLs must be HTTP(S) URLs without credentials, queries or fragments");
            }
        }
        if url::Url::parse(&c.server.public_url)?.path() != "/" {
            bail!("public_url must use the host root (no path prefix)");
        }
        if c.auth.password.as_ref().is_some_and(|s| s.is_empty())
            || c.auth.admin_token.as_ref().is_some_and(|s| s.is_empty())
        {
            bail!("authentication secrets cannot be empty");
        }
        if !c.auth.allow_anonymous_pull && c.auth.password.is_none() {
            bail!("NEXUP_AUTH_PASSWORD is required when anonymous pulls are disabled");
        }
        if !c.auth.dashboard_public && c.auth.admin_token.is_none() {
            bail!("NEXUP_ADMIN_TOKEN is required for a private dashboard");
        }
        if c.cache.cleanup_interval_seconds == 0
            || c.cache.max_metadata_bytes == 0
            || c.server.request_timeout_seconds == 0
        {
            bail!("intervals and metadata size must be positive");
        }
        Ok(c)
    }
}
