pub mod cache;
mod catalog;
pub mod config;
mod dashboard;
mod protocols;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::Engine;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub use cache::App;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard::dashboard))
        .route("/metrics", get(dashboard::metrics))
        .route("/api/status", get(dashboard::status))
        .route("/ui/registries/{registry}", get(catalog::registry))
        .route("/ui/registries/{registry}/{*name}", get(catalog::package))
        .route("/v2/", get(protocols::docker_ping))
        .route("/v2/{*path}", get(protocols::docker))
        .route("/npm/{*path}", get(protocols::npm))
        .route("/pypi/simple/", get(protocols::pypi_root))
        .route("/pypi/simple/{*path}", get(protocols::pypi))
        .route("/pypi/files/{*path}", get(protocols::pypi_file))
        .layer(middleware::from_fn_with_state(app.clone(), authorize))
        .with_state(app)
}

async fn health(State(app): State<Arc<App>>) -> Response {
    match sqlx::query("SELECT 1").execute(&app.db).await {
        Ok(_) => (StatusCode::OK, "ok\n").into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database unavailable\n").into_response(),
    }
}

fn equal(a: &str, b: &str) -> bool {
    bool::from(Sha256::digest(a.as_bytes()).ct_eq(&Sha256::digest(b.as_bytes())))
}

async fn authorize(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    let dashboard =
        matches!(path, "/" | "/metrics" | "/api/status" | "/ui") || path.starts_with("/ui/");
    let auth = &app.config.auth;
    if path == "/health"
        || (dashboard && auth.dashboard_public)
        || (!dashboard && auth.allow_anonymous_pull)
    {
        return next.run(req).await;
    }
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let basic = provided
        .strip_prefix("Basic ")
        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
        .and_then(|v| String::from_utf8(v).ok());
    let valid = if dashboard {
        auth.admin_token.as_ref().is_some_and(|token| {
            provided
                .strip_prefix("Bearer ")
                .is_some_and(|p| equal(p, token))
                || basic
                    .as_ref()
                    .and_then(|b| b.split_once(':'))
                    .is_some_and(|(u, p)| equal(u, "admin") & equal(p, token))
        })
    } else {
        auth.password.as_ref().is_some_and(|password| {
            basic
                .as_ref()
                .and_then(|b| b.split_once(':'))
                .is_some_and(|(u, p)| equal(u, &auth.username) & equal(p, password))
                || provided
                    .strip_prefix("Bearer ")
                    .is_some_and(|p| equal(p, password))
        })
    };
    if valid {
        next.run(req).await
    } else {
        let mut response = (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}))).into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            "Basic realm=\"Nexup\"".parse().unwrap(),
        );
        response.headers_mut().insert(
            "docker-distribution-api-version",
            "registry/2.0".parse().unwrap(),
        );
        response
    }
}

pub struct Error(pub StatusCode, pub String);
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, axum::Json(serde_json::json!({"errors":[{"code": if self.0 == StatusCode::NOT_FOUND {"NAME_UNKNOWN"} else {"UNKNOWN"}, "message":self.1}]}))).into_response()
    }
}
impl From<anyhow::Error> for Error {
    fn from(_e: anyhow::Error) -> Self {
        // reqwest errors can include credential-bearing signed URLs: never log them.
        tracing::error!(
            "upstream or cache operation failed; see dashboard for registry and HTTP status"
        );
        Self(
            StatusCode::BAD_GATEWAY,
            "upstream or cache operation failed".into(),
        )
    }
}

pub(crate) fn response(status: StatusCode, content_type: &str, body: impl Into<Body>) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap()
}
