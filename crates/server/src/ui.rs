//! Static Web UI. Files live in `crates/server/ui` and are compiled into the
//! binary, so `bitrev serve` does not need Node, npm, or a frontend build.

use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;

use crate::AppState;

const INDEX: &str = include_str!("../ui/index.html");
const LOGIN: &str = include_str!("../ui/login.html");
const CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");
const LOGIN_JS: &str = include_str!("../ui/login.js");
const FAVICON: &str = include_str!("../ui/favicon.svg");

pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/", get(index))
        .route("/login", get(login))
        .route("/app.css", get(css))
        .route("/app.js", get(app_js))
        .route("/login.js", get(login_js))
        .route("/favicon.svg", get(favicon))
}

async fn index() -> Html<&'static str> {
    Html(INDEX)
}

async fn login() -> Html<&'static str> {
    Html(LOGIN)
}

async fn css() -> impl IntoResponse {
    asset(CSS, "text/css; charset=utf-8")
}

async fn app_js() -> impl IntoResponse {
    asset(APP_JS, "text/javascript; charset=utf-8")
}

async fn login_js() -> impl IntoResponse {
    asset(LOGIN_JS, "text/javascript; charset=utf-8")
}

async fn favicon() -> impl IntoResponse {
    asset(FAVICON, "image/svg+xml")
}

fn asset(body: &'static str, content_type: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, content_type)], body)
}
