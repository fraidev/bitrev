use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bit_rev::config::ServerConfig;
use bit_rev::session::{Session, SessionOptions};
use http_body_util::BodyExt;
use server::{app, auth_file_path, ensure_password};
use tower::ServiceExt;

struct Running {
    app: axum::Router,
    session: Arc<Session>,
    password: String,
    _dir: tempfile::TempDir,
}

async fn running(password: &str) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let session = Arc::new(
        Session::open(SessionOptions {
            listen_port: 0,
            state_dir: Some(dir.path().to_path_buf()),
            lpd: false,
            pex: false,
            webseed: false,
            ..SessionOptions::default()
        })
        .await
        .unwrap(),
    );
    let config = ServerConfig {
        username: "admin".to_string(),
        password: password.to_string(),
        ..ServerConfig::default()
    };
    let app = app(Arc::clone(&session), config);
    Running {
        app,
        session,
        password: password.to_string(),
        _dir: dir,
    }
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn healthz_is_open_and_has_no_library_data() {
    let running = running("secret").await;
    let response = running
        .app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("set-cookie").is_none());
    let body = String::from_utf8(body_bytes(response).await).unwrap();
    assert_eq!(body, "{\"ok\":true}");
    running.session.shutdown_graceful().await;
}

#[tokio::test]
async fn login_page_and_index_are_reachable_without_a_cookie() {
    let running = running("secret").await;
    for path in ["/", "/login"] {
        let response = running
            .app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
    running.session.shutdown_graceful().await;
}

#[tokio::test]
async fn wrong_password_is_rejected_and_right_password_sets_sid() {
    let running = running("secret").await;

    let rejected = running
        .app
        .clone()
        .oneshot(login_request("admin", "nope"))
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    assert!(rejected.headers().get("set-cookie").is_none());

    let accepted = running
        .app
        .clone()
        .oneshot(login_request("admin", &running.password))
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let cookie = accepted
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("SID="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    assert!(cookie.contains("Max-Age=86400"), "{cookie}");

    let sid = cookie.split(';').next().unwrap();
    let unlocked = running
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/torrents")
                .header("cookie", sid)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(unlocked.status(), StatusCode::FORBIDDEN);
    let refreshed = unlocked
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(refreshed.contains("Max-Age=86400"), "{refreshed}");

    running.session.shutdown_graceful().await;
}

#[tokio::test]
async fn unauthenticated_api_is_forbidden_except_login_and_webapi_version() {
    let running = running("secret").await;

    let locked = running
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/torrents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(locked.status(), StatusCode::FORBIDDEN);

    let login = running
        .app
        .clone()
        .oneshot(login_request("admin", "secret"))
        .await
        .unwrap();
    assert_ne!(login.status(), StatusCode::FORBIDDEN);

    let probe = running
        .app
        .oneshot(
            Request::builder()
                .uri("/api/v2/app/webapiVersion")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(probe.status(), StatusCode::FORBIDDEN);

    running.session.shutdown_graceful().await;
}

#[tokio::test]
async fn default_bind_is_loopback() {
    let config = ServerConfig::default();
    assert_eq!(config.host, "127.0.0.1");
    assert_eq!(config.port, 8080);
}

#[cfg(unix)]
#[tokio::test]
async fn generated_password_file_is_mode_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let password = ensure_password(&ServerConfig::default(), dir.path()).unwrap();
    assert_eq!(password.len(), 24);
    let mode = std::fs::metadata(auth_file_path(dir.path()))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

fn login_request(username: &str, password: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"username\":\"{username}\",\"password\":\"{password}\"}}"
        )))
        .unwrap()
}
