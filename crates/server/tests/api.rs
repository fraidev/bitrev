use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bit_rev::config::ServerConfig;
use bit_rev::session::{Session, SessionOptions};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use server::app;
use testkit::TorrentFixture;
use tower::ServiceExt;

struct Running {
    app: axum::Router,
    session: Arc<Session>,
    dir: tempfile::TempDir,
}

async fn running() -> Running {
    let dir = tempfile::tempdir().unwrap();
    let session = Arc::new(
        Session::open(SessionOptions {
            listen_port: 0,
            state_dir: Some(dir.path().join("state")),
            download_dir: dir.path().join("data"),
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
        password: "secret".to_string(),
        ..ServerConfig::default()
    };
    let app = app(Arc::clone(&session), config);
    Running { app, session, dir }
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("json")
}

fn login_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/login")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"username":"admin","password":"secret"}"#))
        .unwrap()
}

async fn sid(app: &axum::Router) -> String {
    let response = app.clone().oneshot(login_request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

fn with_sid(builder: axum::http::request::Builder, sid: &str, body: Body) -> Request<Body> {
    builder.header("cookie", sid).body(body).unwrap()
}

fn multipart_torrent(bytes: &[u8], save_path: &str) -> (String, Body) {
    let boundary = "----bitrevtestboundary";
    let mut body = Vec::new();
    body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"torrent\"; filename=\"api.torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n").into_bytes());
    body.extend_from_slice(bytes);
    body.extend(format!("\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"save_path\"\r\n\r\n{save_path}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"category\"\r\n\r\ntv\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"tags\"\r\n\r\nhd, api\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"sequential\"\r\n\r\ntrue\r\n--{boundary}--\r\n").into_bytes());
    (
        format!("multipart/form-data; boundary={boundary}"),
        Body::from(body),
    )
}

#[tokio::test]
async fn add_list_pause_delete_and_reject_anon() {
    let running = running().await;
    let sid = sid(&running.app).await;
    let fixture = TorrentFixture::builder()
        .single_file("api.bin", 16 * 1024)
        .piece_length(16 * 1024)
        .seed(0xA45)
        .build();
    let save_path = running.dir.path().join("save").join("api.bin");
    let (content_type, body) =
        multipart_torrent(&fixture.torrent_bytes, save_path.to_str().unwrap());

    let locked = running
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/torrents")
                .header("content-type", &content_type)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(locked.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(locked).await["error"], "forbidden");

    let added = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/torrents")
                .header("content-type", content_type),
            &sid,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(added.status(), StatusCode::CREATED);
    let added_body = body_json(added).await;
    let hash = added_body["id"].as_str().unwrap().to_string();
    assert_eq!(hash.len(), 40);
    assert_eq!(added_body["name"], "api.bin");
    assert_eq!(added_body["info_hash"], hash);
    assert_eq!(added_body["category"], "tv");
    assert_eq!(added_body["tags"], json!(["api", "hd"]));
    assert_eq!(added_body["sequential"], true);
    assert_ne!(added_body["state"], "paused");

    let listed = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder().uri("/api/v1/torrents"),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed_body = body_json(listed).await;
    assert_eq!(listed_body.as_array().unwrap().len(), 1);
    assert_eq!(listed_body[0]["id"], hash);

    let one = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder().uri(format!("/api/v1/torrents/{hash}")),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(one.status(), StatusCode::OK);
    assert_eq!(body_json(one).await["name"], "api.bin");

    let paused = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/torrents/{hash}/pause")),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(body_json(paused).await["state"], "paused");

    let files = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder().uri(format!("/api/v1/torrents/{hash}/files")),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(files.status(), StatusCode::OK);
    let files_body = body_json(files).await;
    assert_eq!(files_body.as_array().unwrap().len(), 1);
    assert_eq!(files_body[0]["priority"], "normal");

    let patched = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder()
                .method("PATCH")
                .uri(format!("/api/v1/torrents/{hash}/files"))
                .header("content-type", "application/json"),
            &sid,
            Body::from(r#"{"files":[{"index":0,"priority":"high"}]}"#),
        ))
        .await
        .unwrap();
    assert_eq!(patched.status(), StatusCode::OK);
    assert_eq!(body_json(patched).await[0]["priority"], "high");

    let categories = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder().uri("/api/v1/categories"),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(categories.status(), StatusCode::OK);
    let categories_body = body_json(categories).await;
    assert!(categories_body
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["name"] == "tv"));

    let created = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/categories")
                .header("content-type", "application/json"),
            &sid,
            Body::from(r#"{"name":"movies","save_path":""}"#),
        ))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(body_json(created).await["name"], "movies");

    let session = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder().uri("/api/v1/session"),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    let session_body = body_json(session).await;
    assert_eq!(session_body["torrents"], 1);
    assert!(!session_body["version"].as_str().unwrap().is_empty());
    assert!(session_body["listen_port"].is_number());

    let removed = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/torrents/{hash}?delete_files=false")),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::OK);

    let after = running
        .app
        .oneshot(with_sid(
            Request::builder().uri("/api/v1/torrents"),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(body_json(after).await.as_array().unwrap().len(), 0);

    running.session.shutdown_graceful().await;
}

#[tokio::test]
async fn bad_hash_is_400_and_missing_torrent_is_404() {
    let running = running().await;
    let sid = sid(&running.app).await;

    let bad = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder().uri("/api/v1/torrents/not-a-hash"),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(bad).await["error"],
        "hash must be 40 hex characters"
    );

    let missing = running
        .app
        .oneshot(with_sid(
            Request::builder().uri("/api/v1/torrents/0123456789abcdef0123456789abcdef01234567"),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    running.session.shutdown_graceful().await;
}

#[tokio::test]
async fn json_magnet_add_and_files_default_empty() {
    let running = running().await;
    let sid = sid(&running.app).await;
    let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Hello";
    let added = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/torrents")
                .header("content-type", "application/json"),
            &sid,
            Body::from(
                serde_json::to_vec(&json!({
                    "magnet": magnet,
                    "paused": true,
                    "save_path": running.dir.path().join("magnet")
                }))
                .unwrap(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(added.status(), StatusCode::CREATED);
    let body = body_json(added).await;
    assert_eq!(body["state"], "paused");
    let hash = body["id"].as_str().unwrap().to_string();
    assert_eq!(hash, "0123456789abcdef0123456789abcdef01234567");

    let paused = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/torrents/{hash}/pause")),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(body_json(paused).await["state"], "paused");

    let files = running
        .app
        .oneshot(with_sid(
            Request::builder().uri(format!("/api/v1/torrents/{hash}/files")),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(files.status(), StatusCode::OK);
    assert_eq!(body_json(files).await.as_array().unwrap().len(), 0);

    running.session.shutdown_graceful().await;
}

#[tokio::test]
async fn sse_yields_added_and_drop_does_not_block_add() {
    let running = running().await;
    let sid = sid(&running.app).await;
    let fixture = TorrentFixture::builder()
        .single_file("event.bin", 16 * 1024)
        .piece_length(16 * 1024)
        .seed(0xE45)
        .build();
    let save_path = running.dir.path().join("events").join("event.bin");

    let response = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder().uri("/api/v1/events"),
            &sid,
            Body::empty(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );

    let (content_type, body) =
        multipart_torrent(&fixture.torrent_bytes, save_path.to_str().unwrap());
    let added = running
        .app
        .clone()
        .oneshot(with_sid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/torrents")
                .header("content-type", content_type),
            &sid,
            body,
        ))
        .await
        .unwrap();
    assert_eq!(added.status(), StatusCode::CREATED);

    let frame = tokio::time::timeout(Duration::from_secs(3), response.into_body().frame())
        .await
        .expect("sse timed out")
        .expect("sse frame")
        .expect("sse frame ok");
    let bytes = frame
        .into_data()
        .unwrap_or_else(|frame| panic!("expected data frame, got {frame:?}"));
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("event: added") || text.contains("\"type\":\"added\""),
        "{text}"
    );

    let started = Instant::now();
    let again = running
        .app
        .oneshot(with_sid(
            Request::builder()
                .method("POST")
                .uri("/api/v1/torrents")
                .header("content-type", "application/json"),
            &sid,
            Body::from(
                r#"{"magnet":"magnet:?xt=urn:btih:abcdefabcdefabcdefabcdefabcdefabcdefabcd&dn=Next","paused":true}"#,
            ),
        ))
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::CREATED);
    assert!(started.elapsed() < Duration::from_secs(2));

    running.session.shutdown_graceful().await;
}
