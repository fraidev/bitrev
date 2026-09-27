use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bit_rev::config::ServerConfig;
use bit_rev::session::{Session, SessionOptions};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use server::app;
use testkit::TorrentFixture;
use tower::ServiceExt;

const MAGNET: &str =
    "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big%20Buck%20Bunny";
const HASH: &str = "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c";

struct Running {
    app: axum::Router,
    dir: tempfile::TempDir,
}

async fn running(compat: bool) -> Running {
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
        username: "admin".into(),
        password: "secret".into(),
        qbittorrent_compat: compat,
        ..ServerConfig::default()
    };
    Running {
        app: app(session, config),
        dir,
    }
}

async fn text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)))
}

fn cookie(response: &axum::response::Response) -> String {
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

async fn call(
    app: &axum::Router,
    sid: Option<&str>,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: impl Into<Body>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(sid) = sid {
        builder = builder.header("cookie", sid);
    }
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    app.clone()
        .oneshot(builder.body(body.into()).unwrap())
        .await
        .unwrap()
}

fn multipart(parts: &[(&str, &str)], file: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let boundary = "----bitrevqb";
    let mut body = Vec::new();
    for (name, value) in parts {
        body.extend(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .into_bytes(),
        );
    }
    if let Some((filename, bytes)) = file {
        body.extend(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"torrents\"; filename=\"{filename}\"\r\nContent-Type: application/x-bittorrent\r\n\r\n")
                .into_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").into_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn sonarr_request_shapes() {
    let running = running(true).await;
    let app = &running.app;

    let anon = call(
        app,
        None,
        "GET",
        "/api/v2/torrents/info",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(anon.status(), StatusCode::FORBIDDEN);

    let probe = call(
        app,
        None,
        "GET",
        "/api/v2/app/webapiVersion",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(probe.status(), StatusCode::OK);
    assert_eq!(text(probe).await, "2.9.3");

    let bad = call(
        app,
        None,
        "POST",
        "/api/v2/auth/login",
        Some("application/x-www-form-urlencoded"),
        "username=admin&password=nope",
    )
    .await;
    assert_eq!(bad.status(), StatusCode::OK);
    assert_eq!(text(bad).await, "Fails.");

    let login = call(
        app,
        None,
        "POST",
        "/api/v2/auth/login",
        Some("application/x-www-form-urlencoded"),
        "username=admin&password=secret",
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    let sid = cookie(&login);
    assert_eq!(text(login).await, "Ok.");

    let steps = [
        ("GET", "/api/v2/app/version", None, ""),
        ("GET", "/api/v2/app/webapiVersion", None, ""),
        ("GET", "/api/v2/app/buildInfo", None, ""),
        ("GET", "/api/v2/app/preferences", None, ""),
        ("GET", "/api/v2/app/defaultSavePath", None, ""),
        ("GET", "/api/v2/rss/items", None, ""),
        ("GET", "/api/v2/rss/rules", None, ""),
        ("GET", "/api/v2/search/plugins", None, ""),
        ("GET", "/api/v2/search/categories", None, ""),
        ("GET", "/api/v2/transfer/info", None, ""),
    ];
    for (method, path, content_type, body) in steps {
        let response = call(app, Some(&sid), method, path, content_type, body).await;
        assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
        let _ = response;
    }

    let version = call(
        app,
        Some(&sid),
        "GET",
        "/api/v2/app/version",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(text(version).await, "v4.6.7");

    let prefs = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/app/preferences",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    for key in [
        "save_path",
        "listen_port",
        "upnp",
        "dht",
        "pex",
        "lsd",
        "max_connec",
        "max_connec_per_torrent",
        "dl_limit",
        "up_limit",
        "max_active_downloads",
        "max_active_uploads",
        "queueing_enabled",
        "web_ui_username",
        "max_ratio_act",
    ] {
        assert!(prefs.get(key).is_some(), "missing preference {key}");
    }
    assert_eq!(prefs["web_ui_username"], "admin");
    assert_eq!(prefs["max_ratio_act"], 0);

    let set_prefs = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/app/setPreferences",
        Some("application/x-www-form-urlencoded"),
        "json=%7B%22dl_limit%22%3A1024%2C%22up_limit%22%3A2048%7D",
    )
    .await;
    assert_eq!(set_prefs.status(), StatusCode::OK);
    let prefs = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/app/preferences",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(prefs["dl_limit"], 1024);
    assert_eq!(prefs["up_limit"], 2048);

    let save = text(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/app/defaultSavePath",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(save, running.dir.path().join("data").to_string_lossy());

    let (content_type, body) = multipart(
        &[
            ("urls", MAGNET),
            ("category", "tv-sonarr"),
            ("paused", "true"),
            ("tags", "sonarr"),
            ("ratioLimit", "4"),
            ("seedingTimeLimit", "1800"),
            ("sequentialDownload", "false"),
            ("firstLastPiecePrio", "false"),
            ("contentLayout", "Original"),
        ],
        None,
    );
    let added = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/add",
        Some(&content_type),
        body,
    )
    .await;
    assert_eq!(added.status(), StatusCode::OK);
    assert_eq!(text(added).await, "Ok.");

    let listed = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/torrents/info?category=tv-sonarr",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let mut got = listed[0].clone();
    assert!(got["added_on"].as_i64().unwrap() > 0);
    got["added_on"] = json!(0);
    let save_path = running.dir.path().join("data");
    let expected = json!({
        "hash": HASH,
        "name": "Big Buck Bunny",
        "state": "pausedDL",
        "progress": 0.0,
        "dlspeed": 0,
        "upspeed": 0,
        "size": 0,
        "amount_left": 0,
        "completed": 0,
        "downloaded": 0,
        "uploaded": 0,
        "ratio": 0.0,
        "save_path": save_path.join("Big Buck Bunny").to_string_lossy(),
        "content_path": save_path.join("Big Buck Bunny").to_string_lossy(),
        "category": "tv-sonarr",
        "tags": "sonarr",
        "seq_dl": false,
        "force_start": false,
        "completion_on": -1,
        "added_on": 0,
        "num_leechs": 0,
        "num_seeds": 0,
        "eta": 8640000,
        "ratio_limit": 4.0,
        "seeding_time_limit": 1800,
    });
    assert_eq!(got, expected);

    let paused = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/pause",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={HASH}"),
    )
    .await;
    assert_eq!(paused.status(), StatusCode::OK);

    let stopped = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/stop",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={HASH}"),
    )
    .await;
    assert_eq!(stopped.status(), StatusCode::OK);

    let resumed = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/resume",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={HASH}"),
    )
    .await;
    assert_eq!(resumed.status(), StatusCode::OK);

    let started = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/start",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={HASH}"),
    )
    .await;
    assert_eq!(started.status(), StatusCode::OK);

    let after_start = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            &format!("/api/v2/torrents/info?hashes={HASH}"),
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(after_start[0]["state"], "metaDL");

    let categorized = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/setCategory",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={HASH}&category=tv-radarr"),
    )
    .await;
    assert_eq!(categorized.status(), StatusCode::OK);

    let limits = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/setShareLimits",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={HASH}&ratioLimit=1.5&seedingTimeLimit=90"),
    )
    .await;
    assert_eq!(limits.status(), StatusCode::OK);

    let updated = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/torrents/info?category=tv-radarr",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(updated.as_array().unwrap().len(), 1);
    assert_eq!(updated[0]["hash"], HASH);
    assert_eq!(updated[0]["category"], "tv-radarr");
    assert_eq!(updated[0]["ratio_limit"], 1.5);
    assert_eq!(updated[0]["seeding_time_limit"], 90);
    let hidden = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/torrents/info?category=tv-sonarr",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(hidden.as_array().unwrap().len(), 0);

    let cats = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/torrents/categories",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(cats["tv-sonarr"]["name"], "tv-sonarr");
    assert_eq!(cats["tv-radarr"]["name"], "tv-radarr");

    let props = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            &format!("/api/v2/torrents/properties?hash={HASH}"),
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(props["name"], "Big Buck Bunny");
    assert_eq!(props["has_metadata"], false);

    let files = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            &format!("/api/v2/torrents/files?hash={HASH}"),
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(files, json!([]));

    let rss = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/rss/items",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(rss, json!([]));
    let rules = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/rss/rules",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(rules, json!({}));
    let plugins = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/search/plugins",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(plugins, json!([]));

    let removed = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/delete",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={HASH}&deleteFiles=true"),
    )
    .await;
    assert_eq!(removed.status(), StatusCode::OK);
    let empty = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/torrents/info",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(empty, json!([]));
}

#[tokio::test]
async fn torrent_file_queue_limits_and_sync() {
    let running = running(true).await;
    let app = &running.app;
    let login = call(
        app,
        None,
        "POST",
        "/api/v2/auth/login",
        Some("application/x-www-form-urlencoded"),
        "username=admin&password=secret",
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    let sid = cookie(&login);

    let fixture = TorrentFixture::builder()
        .single_file("episode.mkv", 32 * 1024)
        .piece_length(16 * 1024)
        .seed(0x46)
        .build();
    let (content_type, body) = multipart(
        &[
            ("category", "tv-sonarr"),
            ("paused", "true"),
            ("skip_checking", "true"),
        ],
        Some(("episode.torrent", &fixture.torrent_bytes)),
    );
    let added = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/add",
        Some(&content_type),
        body,
    )
    .await;
    assert_eq!(added.status(), StatusCode::OK, "{}", text(added).await);

    let listed = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/torrents/info?category=tv-sonarr",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    let hash = listed[0]["hash"].as_str().unwrap().to_string();
    assert_eq!(listed[0]["name"], "episode.mkv");
    assert_eq!(listed[0]["state"], "pausedDL");
    assert_eq!(listed[0]["category"], "tv-sonarr");

    let files = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            &format!("/api/v2/torrents/files?hash={hash}"),
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(files[0]["name"], "episode.mkv");
    assert_eq!(files[0]["priority"], 1);

    let prio = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/filePrio",
        Some("application/x-www-form-urlencoded"),
        format!("hash={hash}&id=0&priority=0"),
    )
    .await;
    assert_eq!(prio.status(), StatusCode::OK);
    let files = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            &format!("/api/v2/torrents/files?hash={hash}"),
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(files[0]["priority"], 0);

    let states = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            &format!("/api/v2/torrents/pieceStates?hash={hash}"),
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(states.as_array().unwrap().len(), 2);

    let forced = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/setForceStart",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={hash}&value=true"),
    )
    .await;
    assert_eq!(forced.status(), StatusCode::OK);
    let info = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            &format!("/api/v2/torrents/info?hashes={hash}"),
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(info[0]["force_start"], true);

    for path in [
        "/api/v2/torrents/topPrio",
        "/api/v2/torrents/bottomPrio",
        "/api/v2/torrents/increasePrio",
        "/api/v2/torrents/decreasePrio",
    ] {
        let response = call(
            app,
            Some(&sid),
            "POST",
            path,
            Some("application/x-www-form-urlencoded"),
            format!("hashes={hash}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }

    let sync = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/sync/maindata?rid=0",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(sync["full_update"], true);
    assert!(sync["torrents"].get(&hash).is_some());
    let rid = sync["rid"].as_u64().unwrap();
    let again = json_body(
        call(
            app,
            Some(&sid),
            "GET",
            &format!("/api/v2/sync/maindata?rid={rid}"),
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(again["full_update"], false);
    assert_eq!(again["torrents"], json!({}));

    let mode = text(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/transfer/speedLimitsMode",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(mode, "0");
    let toggled = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/transfer/toggleSpeedLimitsMode",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(toggled.status(), StatusCode::OK);
    let mode = text(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/transfer/speedLimitsMode",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(mode, "1");
    let back = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/transfer/speedLimitsMode",
        Some("application/x-www-form-urlencoded"),
        "mode=0",
    )
    .await;
    assert_eq!(back.status(), StatusCode::OK);

    let limited = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/transfer/downloadLimit",
        Some("application/x-www-form-urlencoded"),
        "limit=4096",
    )
    .await;
    assert_eq!(limited.status(), StatusCode::OK);
    let got = text(
        call(
            app,
            Some(&sid),
            "GET",
            "/api/v2/transfer/downloadLimit",
            None,
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(got, "4096");

    let created = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/createCategory",
        Some("application/x-www-form-urlencoded"),
        "category=tv-sonarr&savePath=/tv",
    )
    .await;
    assert_eq!(created.status(), StatusCode::CONFLICT);

    let removed = call(
        app,
        Some(&sid),
        "POST",
        "/api/v2/torrents/delete",
        Some("application/x-www-form-urlencoded"),
        format!("hashes={hash}&deleteFiles=true"),
    )
    .await;
    assert_eq!(removed.status(), StatusCode::OK);
}

#[tokio::test]
async fn compat_flag_unmounts_api_v2() {
    let running = running(false).await;
    let login = call(
        &running.app,
        None,
        "POST",
        "/api/v1/login",
        Some("application/json"),
        r#"{"username":"admin","password":"secret"}"#,
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    let sid = cookie(&login);
    let missing = call(
        &running.app,
        Some(&sid),
        "GET",
        "/api/v2/app/version",
        None,
        Body::empty(),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let probe = call(
        &running.app,
        None,
        "GET",
        "/api/v2/app/webapiVersion",
        None,
        Body::empty(),
    )
    .await;
    assert_ne!(probe.status(), StatusCode::FORBIDDEN);
    assert_eq!(probe.status(), StatusCode::NOT_FOUND);
}
