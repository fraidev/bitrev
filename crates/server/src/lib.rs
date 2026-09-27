//! HTTP shell for `bitrev serve`.
//!
//! Routes for the native API, qBittorrent compatibility, and the Web UI land
//! in later issues. This crate owns the process bind, cookie auth, and
//! `GET /healthz`.
//!
//! Request logs are `tracing` spans named `http` with `method`, `path`,
//! `status`, and `latency`. The CLI installs a `tracing-subscriber` filter
//! from `RUST_LOG` (default `info` when the variable is unset). Examples:
//!
//! ```text
//! RUST_LOG=info bitrev serve
//! RUST_LOG=debug bitrev serve
//! RUST_LOG=tower_http=debug,server=debug bitrev serve
//! ```

mod auth;
mod password;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::Request;
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use bit_rev::config::ServerConfig;
use bit_rev::session::Session;
use serde_json::json;
use tower_http::trace::TraceLayer;
use tracing::Span;

pub use auth::{require_api_auth, SessionStore, COOKIE_NAME, SESSION_TTL};
pub use password::{auth_file_path, ensure_password, AUTH_FILE_NAME};

#[derive(Clone)]
pub struct AppState {
    pub session: Arc<Session>,
    pub config: ServerConfig,
    pub sessions: Arc<SessionStore>,
}

/// Router used by `bitrev serve` and by in-process tests.
///
/// `config.password` is the password login accepts. Call [`ensure_password`]
/// first when the configured password is empty.
pub fn app(session: Arc<Session>, config: ServerConfig) -> Router {
    let state = AppState {
        session,
        config,
        sessions: Arc::new(SessionStore::default()),
    };
    Router::new()
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .route("/login", get(login_page))
        .route("/api/v1/login", post(auth::login))
        .route("/api/v1/logout", post(auth::logout))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_api_auth,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<_>| {
                    tracing::info_span!(
                        "http",
                        method = %request.method(),
                        path = request.uri().path(),
                        status = tracing::field::Empty,
                        latency = tracing::field::Empty,
                    )
                })
                .on_response(
                    |response: &axum::response::Response, latency: Duration, span: &Span| {
                        span.record("status", response.status().as_u16());
                        span.record("latency", latency.as_millis());
                    },
                ),
        )
        .with_state(state)
}

/// Bind `addr` and run until `shutdown` completes, then drain in-flight HTTP.
pub async fn serve(
    router: Router,
    addr: SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(%bound, "http listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

async fn index(State(state): State<AppState>) -> Html<String> {
    let port = state.session.listen_port();
    Html(format!(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>bitrev</title></head>\
<body><p>bitrev is running. Engine listen port {port}.</p><p><a href=\"/login\">Log in</a></p></body></html>\n"
    ))
}

async fn login_page() -> Html<&'static str> {
    Html(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>bitrev login</title></head>\
<body><h1>bitrev</h1>\
<form method=\"post\" action=\"/api/v1/login\">\
<label>Username <input name=\"username\" autocomplete=\"username\"></label>\
<label>Password <input name=\"password\" type=\"password\" autocomplete=\"current-password\"></label>\
<button type=\"submit\">Log in</button>\
</form></body></html>\n",
    )
}
