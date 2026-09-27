use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use rand::Rng;
use serde::Deserialize;
use serde_json::json;

use crate::password::constant_time_eq;
use crate::AppState;

pub const COOKIE_NAME: &str = "SID";
pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_AGE_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, Default)]
pub struct SessionStore {
    entries: Mutex<HashMap<String, Instant>>,
}

impl SessionStore {
    pub fn insert(&self, sid: &str) {
        self.entries
            .lock()
            .expect("session store")
            .insert(sid.to_string(), Instant::now() + SESSION_TTL);
    }

    pub fn touch(&self, sid: &str) -> bool {
        let mut entries = self.entries.lock().expect("session store");
        let Some(expiry) = entries.get(sid).copied() else {
            return false;
        };
        if expiry <= Instant::now() {
            entries.remove(sid);
            return false;
        }
        entries.insert(sid.to_string(), Instant::now() + SESSION_TTL);
        true
    }

    pub fn remove(&self, sid: &str) {
        self.entries.lock().expect("session store").remove(sid);
    }
}

pub fn is_public_api(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/login"
            | "/api/v1/logout"
            | "/api/v2/auth/login"
            | "/api/v2/auth/logout"
            | "/api/v2/app/webapiVersion"
    )
}

pub async fn require_api_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    if !path.starts_with("/api/") || is_public_api(&path) {
        return next.run(request).await;
    }

    let Some(sid) = cookie_value(request.headers(), COOKIE_NAME) else {
        return forbidden();
    };
    if !state.sessions.touch(&sid) {
        return forbidden();
    }

    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&session_cookie(&sid)) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

pub async fn login(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(creds) = parse_credentials(&headers, &body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username and password are required"})),
        )
            .into_response();
    };

    let user_ok = constant_time_eq(creds.username.as_bytes(), state.config.username.as_bytes());
    let pass_ok = constant_time_eq(creds.password.as_bytes(), state.config.password.as_bytes());
    if !user_ok || !pass_ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid credentials"})),
        )
            .into_response();
    }

    let sid = new_sid();
    state.sessions.insert(&sid);
    let mut response = if is_form(&headers) {
        axum::response::Redirect::to("/").into_response()
    } else {
        Json(json!({"ok": true})).into_response()
    };
    if let Ok(value) = HeaderValue::from_str(&session_cookie(&sid)) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(sid) = cookie_value(&headers, COOKIE_NAME) {
        state.sessions.remove(&sid);
    }
    let mut response = Json(json!({"ok": true})).into_response();
    if let Ok(value) = HeaderValue::from_str(&clear_cookie()) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

#[derive(Debug, Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

fn is_form(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .starts_with("application/x-www-form-urlencoded")
}

fn parse_credentials(headers: &HeaderMap, body: &[u8]) -> Option<Credentials> {
    if is_form(headers) {
        return serde_urlencoded::from_bytes(body).ok();
    }
    serde_json::from_slice(body).ok()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden"}))).into_response()
}

fn new_sid() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn session_cookie(sid: &str) -> String {
    format!("{COOKIE_NAME}={sid}; HttpOnly; SameSite=Lax; Path=/; Max-Age={MAX_AGE_SECS}")
}

fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in header.split(';') {
        let mut item = part.trim().splitn(2, '=');
        if item.next()? == name {
            return Some(item.next().unwrap_or("").to_string());
        }
    }
    None
}
