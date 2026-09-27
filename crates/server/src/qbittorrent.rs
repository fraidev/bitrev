//! qBittorrent Web API v2 facade. Sonarr, Radarr, and the rest of the *arr
//! stack speak this protocol. Every mutation goes through [`Session`].
//!
//! Identity is qBittorrent 4.6.7 / WebAPI 2.9.3 so clients keep using
//! `pause` / `resume`. `stop` / `start` are aliases.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{FromRequest, Multipart, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bit_rev::file;
use bit_rev::magnet::Magnet;
use bit_rev::priority::{FileInfo, FilePriority};
use bit_rev::session::{AddTorrentOptions, ControlError, TorrentId, TorrentSnapshot, TorrentState};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::auth::{cookie_value, session_cookie, COOKIE_NAME};
use crate::password::constant_time_eq;
use crate::AppState;

const QB_VERSION: &str = "v4.6.7";
const WEBAPI_VERSION: &str = "2.9.3";
const ETA_INFINITY: i64 = 8_640_000;

pub struct SyncCache {
    inner: Mutex<SyncInner>,
}

struct SyncInner {
    rid: u64,
    torrents: BTreeMap<String, Value>,
    categories: BTreeMap<String, Value>,
}

impl Default for SyncCache {
    fn default() -> Self {
        Self {
            inner: Mutex::new(SyncInner {
                rid: 0,
                torrents: BTreeMap::new(),
                categories: BTreeMap::new(),
            }),
        }
    }
}

impl SyncCache {
    fn maindata(
        &self,
        client_rid: u64,
        torrents: BTreeMap<String, Value>,
        categories: BTreeMap<String, Value>,
        tags: Vec<String>,
        server_state: Value,
    ) -> Value {
        let mut inner = self.inner.lock().expect("sync cache");
        let full = client_rid == 0 || client_rid != inner.rid;
        inner.rid = inner.rid.saturating_add(1);
        let rid = inner.rid;
        if full {
            inner.torrents.clone_from(&torrents);
            inner.categories.clone_from(&categories);
            return json!({
                "rid": rid,
                "full_update": true,
                "torrents": torrents,
                "categories": categories,
                "tags": tags,
                "server_state": server_state,
            });
        }

        let mut changed = Map::new();
        let mut removed = Vec::new();
        for hash in inner.torrents.keys() {
            if !torrents.contains_key(hash) {
                removed.push(hash.clone());
            }
        }
        for (hash, current) in &torrents {
            if inner.torrents.get(hash) != Some(current) {
                changed.insert(hash.clone(), current.clone());
            }
        }
        let mut categories_changed = Map::new();
        let mut categories_removed = Vec::new();
        for name in inner.categories.keys() {
            if !categories.contains_key(name) {
                categories_removed.push(name.clone());
            }
        }
        for (name, current) in &categories {
            if inner.categories.get(name) != Some(current) {
                categories_changed.insert(name.clone(), current.clone());
            }
        }
        inner.torrents = torrents;
        inner.categories = categories;
        json!({
            "rid": rid,
            "full_update": false,
            "torrents": Value::Object(changed),
            "torrents_removed": removed,
            "categories": Value::Object(categories_changed),
            "categories_removed": categories_removed,
            "server_state": server_state,
        })
    }
}

pub fn mount(router: Router<AppState>, enabled: bool) -> Router<AppState> {
    if !enabled {
        return router;
    }
    router
        .route("/api/v2/auth/login", post(login))
        .route("/api/v2/auth/logout", post(logout))
        .route("/api/v2/app/version", get(version))
        .route("/api/v2/app/webapiVersion", get(webapi_version))
        .route("/api/v2/app/buildInfo", get(build_info))
        .route("/api/v2/app/preferences", get(preferences))
        .route("/api/v2/app/setPreferences", post(set_preferences))
        .route("/api/v2/app/defaultSavePath", get(default_save_path))
        .route("/api/v2/torrents/info", get(torrents_info))
        .route("/api/v2/torrents/properties", get(torrent_properties))
        .route("/api/v2/torrents/files", get(torrent_files))
        .route("/api/v2/torrents/pieceStates", get(piece_states))
        .route("/api/v2/torrents/trackers", get(torrent_trackers))
        .route("/api/v2/torrents/add", post(add_torrent))
        .route("/api/v2/torrents/delete", post(delete_torrents))
        .route("/api/v2/torrents/pause", post(pause_torrents))
        .route("/api/v2/torrents/stop", post(pause_torrents))
        .route("/api/v2/torrents/resume", post(resume_torrents))
        .route("/api/v2/torrents/start", post(resume_torrents))
        .route("/api/v2/torrents/recheck", post(recheck_torrents))
        .route("/api/v2/torrents/setCategory", post(set_category))
        .route("/api/v2/torrents/createCategory", post(create_category))
        .route("/api/v2/torrents/categories", get(categories))
        .route("/api/v2/torrents/removeCategories", post(remove_categories))
        .route("/api/v2/torrents/addTags", post(add_tags))
        .route("/api/v2/torrents/removeTags", post(remove_tags))
        .route("/api/v2/torrents/setTags", post(set_tags))
        .route("/api/v2/torrents/tags", get(empty_list))
        .route("/api/v2/torrents/setShareLimits", post(set_share_limits))
        .route("/api/v2/torrents/setForceStart", post(set_force_start))
        .route("/api/v2/torrents/setLocation", post(set_location))
        .route("/api/v2/torrents/filePrio", post(file_prio))
        .route("/api/v2/torrents/topPrio", post(queue_top))
        .route("/api/v2/torrents/bottomPrio", post(queue_bottom))
        .route("/api/v2/torrents/increasePrio", post(queue_up))
        .route("/api/v2/torrents/decreasePrio", post(queue_down))
        .route("/api/v2/transfer/info", get(transfer_info))
        .route(
            "/api/v2/transfer/downloadLimit",
            get(download_limit).post(set_download_limit),
        )
        .route(
            "/api/v2/transfer/setDownloadLimit",
            post(set_download_limit),
        )
        .route(
            "/api/v2/transfer/uploadLimit",
            get(upload_limit).post(set_upload_limit),
        )
        .route("/api/v2/transfer/setUploadLimit", post(set_upload_limit))
        .route(
            "/api/v2/transfer/speedLimitsMode",
            get(speed_limits_mode).post(set_speed_limits_mode),
        )
        .route(
            "/api/v2/transfer/toggleSpeedLimitsMode",
            post(toggle_speed_limits_mode),
        )
        .route("/api/v2/sync/maindata", get(sync_maindata))
        .route("/api/v2/rss/rules", get(empty_object))
        .route("/api/v2/rss/items", get(empty_list))
        .route("/api/v2/rss/{*path}", get(empty_list))
        .route("/api/v2/search/plugins", get(empty_list))
        .route("/api/v2/search/categories", get(empty_list))
        .route("/api/v2/search/{*path}", get(empty_list))
}

fn plain(body: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        body.into(),
    )
        .into_response()
}

fn fail(status: StatusCode) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "Fails.",
    )
        .into_response()
}

fn ok_body() -> Response {
    plain("Ok.")
}

fn json_ok(value: Value) -> Response {
    Json(value).into_response()
}

fn with_cookie(mut response: Response, cookie: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

fn new_sid() -> String {
    let mut bytes = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

async fn login(State(state): State<AppState>, body: Bytes) -> Response {
    let Some(creds) = serde_urlencoded::from_bytes::<Credentials>(&body).ok() else {
        return plain("Fails.");
    };
    let user_ok = constant_time_eq(creds.username.as_bytes(), state.config.username.as_bytes());
    let pass_ok = constant_time_eq(creds.password.as_bytes(), state.config.password.as_bytes());
    if !user_ok || !pass_ok {
        return plain("Fails.");
    }
    let sid = new_sid();
    state.sessions.insert(&sid);
    with_cookie(plain("Ok."), &session_cookie(&sid))
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(sid) = cookie_value(&headers, COOKIE_NAME) {
        state.sessions.remove(&sid);
    }
    with_cookie(plain("Ok."), &clear_cookie())
}

async fn version() -> Response {
    plain(QB_VERSION)
}

async fn webapi_version() -> Response {
    plain(WEBAPI_VERSION)
}

async fn build_info() -> Response {
    json_ok(json!({
        "qt": "6.4.3",
        "libtorrent": "1.2.19.0",
        "boost": "1.82.0",
        "openssl": "3.0.12",
        "bitness": usize::BITS,
        "zlib": "1.3",
    }))
}

async fn preferences(State(state): State<AppState>) -> Response {
    json_ok(preference_json(&state))
}

fn preference_json(state: &AppState) -> Value {
    let opts = state.session.options();
    let (download, upload) = state.session.normal_rate_limits();
    let stats = state.session.transfer_stats();
    let queueing =
        opts.max_active_downloads > 0 || opts.max_active_uploads > 0 || opts.max_active > 0;
    json!({
        "save_path": path_string(&opts.download_dir),
        "listen_port": state.session.listen_port(),
        "upnp": opts.nat.enabled,
        "dht": opts.dht.enabled,
        "pex": opts.pex,
        "lsd": opts.lpd,
        "max_connec": opts.max_peers_global,
        "max_connec_per_torrent": opts.max_peers_per_torrent,
        "dl_limit": download,
        "up_limit": upload,
        "alt_dl_limit": stats.alt_download_limit,
        "alt_up_limit": stats.alt_upload_limit,
        "max_active_downloads": opts.max_active_downloads,
        "max_active_uploads": opts.max_active_uploads,
        "queueing_enabled": queueing,
        "web_ui_username": state.config.username,
        "max_ratio_enabled": opts.seed_ratio_limit > 0.0,
        "max_ratio": opts.seed_ratio_limit,
        "max_seeding_time_enabled": opts.seed_time_limit > 0,
        "max_seeding_time": opts.seed_time_limit,
        "max_ratio_act": 0,
    })
}

async fn set_preferences(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(raw) = map.get("json") else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let prefs: Map<String, Value> = match serde_json::from_str(raw) {
        Ok(prefs) => prefs,
        Err(_) => return fail(StatusCode::BAD_REQUEST),
    };
    let (mut download, mut upload) = state.session.normal_rate_limits();
    let mut rates = false;
    if let Some(value) = prefs.get("dl_limit") {
        let Some(limit) = json_limit(value) else {
            return fail(StatusCode::BAD_REQUEST);
        };
        download = limit;
        rates = true;
    }
    if let Some(value) = prefs.get("up_limit") {
        let Some(limit) = json_limit(value) else {
            return fail(StatusCode::BAD_REQUEST);
        };
        upload = limit;
        rates = true;
    }
    if rates {
        state.session.set_rate_limits(download, upload);
    }
    let stats = state.session.transfer_stats();
    let mut alt_download = stats.alt_download_limit;
    let mut alt_upload = stats.alt_upload_limit;
    let mut alts = false;
    if let Some(value) = prefs.get("alt_dl_limit") {
        let Some(limit) = json_limit(value) else {
            return fail(StatusCode::BAD_REQUEST);
        };
        alt_download = limit;
        alts = true;
    }
    if let Some(value) = prefs.get("alt_up_limit") {
        let Some(limit) = json_limit(value) else {
            return fail(StatusCode::BAD_REQUEST);
        };
        alt_upload = limit;
        alts = true;
    }
    if alts {
        state.session.set_alt_limits(alt_download, alt_upload);
    }
    ok_body()
}

async fn default_save_path(State(state): State<AppState>) -> Response {
    plain(path_string(&state.session.options().download_dir))
}

async fn torrents_info(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let snaps = select_torrents(&state, &query);
    json_ok(Value::Array(snaps.iter().map(info_json).collect()))
}

async fn torrent_properties(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(snap) = one_snapshot(&state, query.get("hash").map(String::as_str)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let peers = snap.peers.saturating_sub(snap.seeds);
    json_ok(json!({
        "save_path": path_string(&snap.save_path),
        "addition_date": snap.added_at,
        "completion_date": snap.completed_at.unwrap_or(-1),
        "creation_date": -1,
        "piece_size": snap.piece_length,
        "pieces_num": snap.piece_count,
        "pieces_have": snap.pieces_have,
        "comment": "",
        "created_by": "",
        "total_size": snap.size,
        "total_downloaded": snap.downloaded,
        "total_downloaded_session": snap.downloaded,
        "total_uploaded": snap.uploaded,
        "total_uploaded_session": snap.uploaded,
        "total_wasted": 0,
        "dl_speed": snap.download_rate,
        "up_speed": snap.upload_rate,
        "dl_speed_avg": snap.download_rate,
        "up_speed_avg": snap.upload_rate,
        "dl_limit": -1,
        "up_limit": -1,
        "eta": eta(&snap),
        "share_ratio": snap.ratio,
        "seeds": snap.seeds,
        "seeds_total": snap.seeds,
        "peers": peers,
        "peers_total": snap.peers,
        "nb_connections": snap.peers,
        "nb_connections_limit": state.session.options().max_peers_per_torrent,
        "time_elapsed": 0,
        "seeding_time": 0,
        "reannounce": 0,
        "last_seen": snap.added_at,
        "is_private": false,
        "private": false,
        "has_metadata": snap.state != TorrentState::Metadata && snap.piece_count > 0,
        "progress": snap.progress,
        "name": snap.name,
    }))
}

async fn torrent_files(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(id) = query.get("hash").and_then(|hash| parse_id(hash).ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state.session.files(id) {
        Ok(files) => json_ok(Value::Array(files.iter().map(file_json).collect())),
        Err(ControlError::NoMetadata(_)) => json_ok(json!([])),
        Err(ControlError::NotFound(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => json_ok(json!([])),
    }
}

async fn piece_states(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(id) = query.get("hash").and_then(|hash| parse_id(hash).ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state.session.piece_states(id) {
        Ok(states) => json_ok(json!(states)),
        Err(ControlError::NotFound(_)) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => json_ok(json!([])),
    }
}

async fn torrent_trackers(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(id) = query.get("hash").and_then(|hash| parse_id(hash).ok()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if state.session.snapshot(id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    json_ok(json!([]))
}

async fn add_torrent(State(state): State<AppState>, req: Request) -> Response {
    let content_type = content_type_of(&req);
    let fields = if content_type.starts_with("multipart/") {
        match read_add_multipart(req, &state).await {
            Ok(fields) => fields,
            Err(()) => return fail(StatusCode::BAD_REQUEST),
        }
    } else {
        let map = match read_form(req, &state).await {
            Ok(map) => map,
            Err(()) => return fail(StatusCode::BAD_REQUEST),
        };
        match AddFields::from_map(&map) {
            Ok(fields) => fields,
            Err(()) => return fail(StatusCode::BAD_REQUEST),
        }
    };
    if fields.urls.trim().is_empty() && fields.torrents.is_empty() {
        return fail(StatusCode::BAD_REQUEST);
    }
    let mut added = 0usize;
    for bytes in &fields.torrents {
        if let Ok(meta) = file::from_bytes(bytes) {
            let options = apply_add_fields(AddTorrentOptions::from(meta), &fields);
            if state.session.add_torrent(options).await.is_ok() {
                added += 1;
            }
        }
    }
    for url in split_urls(&fields.urls) {
        let options = if url.starts_with("magnet:") {
            match Magnet::parse(&url) {
                Ok(magnet) => AddTorrentOptions::from_magnet(&magnet),
                Err(_) => continue,
            }
        } else if url.starts_with("http://") || url.starts_with("https://") {
            let Ok(bytes) = fetch_torrent(&url).await else {
                continue;
            };
            match file::from_bytes(&bytes) {
                Ok(meta) => AddTorrentOptions::from(meta),
                Err(_) => continue,
            }
        } else {
            continue;
        };
        let options = apply_add_fields(options, &fields);
        if state.session.add_torrent(options).await.is_ok() {
            added += 1;
        }
    }
    if added == 0 {
        return plain("Fails.");
    }
    ok_body()
}

async fn delete_torrents(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(ids) = ids_from(&state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let delete_files = map
        .get("deleteFiles")
        .and_then(|value| parse_bool(value))
        .unwrap_or(false);
    for id in ids {
        let _ = state.session.remove_torrent(id, delete_files);
    }
    ok_body()
}

async fn pause_torrents(State(state): State<AppState>, req: Request) -> Response {
    mutate_hashes(&state, req, |id| state.session.pause(id).is_ok()).await
}

async fn resume_torrents(State(state): State<AppState>, req: Request) -> Response {
    mutate_hashes(&state, req, |id| state.session.resume(id).is_ok()).await
}

async fn recheck_torrents(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(ids) = ids_from(&state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    for id in ids {
        let _ = state.session.recheck(id).await;
    }
    ok_body()
}

async fn set_category(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(category) = map.get("category") else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let Some(ids) = ids_from(&state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    for id in ids {
        let _ = state.session.set_category(id, category.trim());
    }
    ok_body()
}

async fn create_category(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let name = map.get("category").map(|value| value.trim()).unwrap_or("");
    if name.is_empty() {
        return fail(StatusCode::BAD_REQUEST);
    }
    let save_path = map
        .get("savePath")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    match state.session.create_category(name, save_path) {
        Ok(()) => ok_body(),
        Err(ControlError::CategoryExists(_)) => StatusCode::CONFLICT.into_response(),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn categories(State(state): State<AppState>) -> Response {
    json_ok(Value::Object(category_map(&state).into_iter().collect()))
}

async fn remove_categories(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(raw) = map.get("categories") else {
        return fail(StatusCode::BAD_REQUEST);
    };
    for name in split_list(raw) {
        let _ = state.session.remove_category(&name);
    }
    ok_body()
}

async fn add_tags(State(state): State<AppState>, req: Request) -> Response {
    tags_op(&state, req, TagOp::Add).await
}

async fn remove_tags(State(state): State<AppState>, req: Request) -> Response {
    tags_op(&state, req, TagOp::Remove).await
}

async fn set_tags(State(state): State<AppState>, req: Request) -> Response {
    tags_op(&state, req, TagOp::Set).await
}

#[derive(Clone, Copy)]
enum TagOp {
    Add,
    Remove,
    Set,
}

async fn tags_op(state: &AppState, req: Request, op: TagOp) -> Response {
    let map = match read_form(req, state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(ids) = ids_from(state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let tags = map
        .get("tags")
        .map(|value| split_tags(value))
        .unwrap_or_default();
    for id in ids {
        let _ = match op {
            TagOp::Add => state.session.add_tags(id, tags.clone()),
            TagOp::Remove => state.session.remove_tags(id, tags.clone()),
            TagOp::Set => state.session.set_tags(id, tags.clone()),
        };
    }
    ok_body()
}

async fn set_share_limits(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(ids) = ids_from(&state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let ratio = match map.get("ratioLimit") {
        Some(value) => match parse_ratio_limit(value) {
            Some(limit) => Some(limit),
            None => return fail(StatusCode::BAD_REQUEST),
        },
        None => None,
    };
    let seeding = match map.get("seedingTimeLimit") {
        Some(value) => match parse_seed_minutes(value) {
            Some(limit) => Some(limit),
            None => return fail(StatusCode::BAD_REQUEST),
        },
        None => None,
    };
    for id in ids {
        let Some(snap) = state.session.snapshot(id) else {
            continue;
        };
        let ratio_limit = ratio.unwrap_or(snap.ratio_limit);
        let seeding_time_limit = seeding.unwrap_or(snap.seeding_time_limit);
        let _ = state
            .session
            .set_share_limits(id, ratio_limit, seeding_time_limit);
    }
    ok_body()
}

async fn set_force_start(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(enabled) = map.get("value").and_then(|value| parse_bool(value)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let Some(ids) = ids_from(&state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    for id in ids {
        let _ = state.session.set_force_start(id, enabled);
    }
    ok_body()
}

async fn set_location(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(location) = map.get("location").map(|value| value.trim().to_string()) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    if location.is_empty() {
        return fail(StatusCode::BAD_REQUEST);
    }
    let Some(ids) = ids_from(&state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    for id in ids {
        let _ = state
            .session
            .set_save_path(id, PathBuf::from(&location), true)
            .await;
    }
    ok_body()
}

async fn file_prio(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(hash) = map.get("hash") else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let Ok(id) = parse_id(hash) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let Some(priority) = map
        .get("priority")
        .and_then(|value| value.parse::<i64>().ok())
    else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let Some(ids) = map.get("id") else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let priority = from_qb_prio(priority);
    for index in split_list(ids) {
        let Ok(index) = index.parse::<usize>() else {
            continue;
        };
        let _ = state.session.set_file_priority(id, index, priority);
    }
    ok_body()
}

async fn queue_top(State(state): State<AppState>, req: Request) -> Response {
    queue_op(&state, req, QueueOp::Top).await
}

async fn queue_bottom(State(state): State<AppState>, req: Request) -> Response {
    queue_op(&state, req, QueueOp::Bottom).await
}

async fn queue_up(State(state): State<AppState>, req: Request) -> Response {
    queue_op(&state, req, QueueOp::Up).await
}

async fn queue_down(State(state): State<AppState>, req: Request) -> Response {
    queue_op(&state, req, QueueOp::Down).await
}

#[derive(Clone, Copy)]
enum QueueOp {
    Top,
    Bottom,
    Up,
    Down,
}

async fn queue_op(state: &AppState, req: Request, op: QueueOp) -> Response {
    let map = match read_form(req, state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(ids) = ids_from(state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    for id in ids {
        let _ = match op {
            QueueOp::Top => state.session.queue_top(id),
            QueueOp::Bottom => state.session.queue_bottom(id),
            QueueOp::Up => state.session.queue_up(id),
            QueueOp::Down => state.session.queue_down(id),
        };
    }
    ok_body()
}

async fn transfer_info(State(state): State<AppState>) -> Response {
    json_ok(server_state(&state))
}

async fn download_limit(State(state): State<AppState>) -> Response {
    plain(state.session.transfer_stats().download_limit.to_string())
}

async fn upload_limit(State(state): State<AppState>) -> Response {
    plain(state.session.transfer_stats().upload_limit.to_string())
}

async fn set_download_limit(State(state): State<AppState>, req: Request) -> Response {
    set_one_limit(&state, req, true).await
}

async fn set_upload_limit(State(state): State<AppState>, req: Request) -> Response {
    set_one_limit(&state, req, false).await
}

async fn set_one_limit(state: &AppState, req: Request, download: bool) -> Response {
    let map = match read_form(req, state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(limit) = map.get("limit").and_then(|value| parse_byte_limit(value)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    let (mut dl, mut ul) = state.session.normal_rate_limits();
    if download {
        dl = limit;
    } else {
        ul = limit;
    }
    state.session.set_rate_limits(dl, ul);
    ok_body()
}

async fn speed_limits_mode(State(state): State<AppState>) -> Response {
    plain(if state.session.alt_mode() { "1" } else { "0" })
}

async fn toggle_speed_limits_mode(State(state): State<AppState>) -> Response {
    state.session.set_alt_mode(!state.session.alt_mode());
    ok_body()
}

async fn set_speed_limits_mode(State(state): State<AppState>, req: Request) -> Response {
    let map = match read_form(req, &state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    if let Some(mode) = map.get("mode").or_else(|| map.get("value")) {
        let Some(on) = parse_bool(mode).or_else(|| match mode.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        }) else {
            return fail(StatusCode::BAD_REQUEST);
        };
        state.session.set_alt_mode(on);
    } else {
        state.session.set_alt_mode(!state.session.alt_mode());
    }
    ok_body()
}

async fn sync_maindata(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let client_rid = query
        .get("rid")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let torrents = state
        .session
        .list()
        .into_iter()
        .map(|snap| (snap.id.to_string(), info_json(&snap)))
        .collect();
    let categories = category_map(&state);
    let tags = tag_list(&state);
    json_ok(
        state
            .qbit_sync
            .maindata(client_rid, torrents, categories, tags, server_state(&state)),
    )
}

async fn empty_list() -> Json<Vec<Value>> {
    Json(Vec::new())
}

async fn empty_object() -> Json<Value> {
    Json(json!({}))
}

fn server_state(state: &AppState) -> Value {
    let stats = state.session.transfer_stats();
    let nodes = state
        .session
        .dht_stats()
        .map(|stats| stats.nodes)
        .unwrap_or(0);
    json!({
        "dl_info_speed": stats.download_rate,
        "dl_info_data": stats.downloaded,
        "up_info_speed": stats.upload_rate,
        "up_info_data": stats.uploaded,
        "dl_rate_limit": stats.download_limit,
        "up_rate_limit": stats.upload_limit,
        "dht_nodes": nodes,
        "connection_status": connection_status(stats.port_open),
        "use_alt_speed_limits": stats.alt_mode,
        "queued_io_jobs": 0,
        "refresh_interval": 1500,
    })
}

fn connection_status(port_open: Option<bool>) -> &'static str {
    if port_open == Some(false) {
        "firewalled"
    } else {
        "connected"
    }
}

fn category_map(state: &AppState) -> BTreeMap<String, Value> {
    state
        .session
        .categories()
        .into_iter()
        .map(|category| {
            let save_path = category
                .save_path
                .as_ref()
                .map(|path| path_string(path))
                .unwrap_or_default();
            (
                category.name.clone(),
                json!({
                    "name": category.name,
                    "savePath": save_path,
                }),
            )
        })
        .collect()
}

fn tag_list(state: &AppState) -> Vec<String> {
    let mut tags = BTreeMap::new();
    for snap in state.session.list() {
        for tag in snap.tags {
            tags.insert(tag, ());
        }
    }
    tags.into_keys().collect()
}

async fn mutate_hashes(
    state: &AppState,
    req: Request,
    mut op: impl FnMut(TorrentId) -> bool,
) -> Response {
    let map = match read_form(req, state).await {
        Ok(map) => map,
        Err(()) => return fail(StatusCode::BAD_REQUEST),
    };
    let Some(ids) = ids_from(state, map.get("hashes").map(String::as_str)) else {
        return fail(StatusCode::BAD_REQUEST);
    };
    for id in ids {
        op(id);
    }
    ok_body()
}

fn ids_from(state: &AppState, raw: Option<&str>) -> Option<Vec<TorrentId>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == "all" {
        return Some(
            state
                .session
                .list()
                .into_iter()
                .map(|snap| snap.id)
                .collect(),
        );
    }
    let mut ids = Vec::new();
    for part in raw.split('|') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Ok(id) = parse_id(part) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

fn select_torrents(state: &AppState, query: &HashMap<String, String>) -> Vec<TorrentSnapshot> {
    let filter = query.get("filter").map(String::as_str).unwrap_or("all");
    let hashes = if let Some(raw) = query.get("hashes") {
        match ids_from(state, Some(raw)) {
            Some(ids) => Some(ids),
            None => return Vec::new(),
        }
    } else {
        None
    };
    let mut snaps: Vec<TorrentSnapshot> = state
        .session
        .list()
        .into_iter()
        .filter(|snap| passes_filter(snap, filter))
        .filter(|snap| match query.get("category") {
            Some(category) => snap.category == *category,
            None => true,
        })
        .filter(|snap| match query.get("tag") {
            Some(tag) => snap.tags.iter().any(|have| have == tag),
            None => true,
        })
        .filter(|snap| match &hashes {
            Some(ids) => ids.contains(&snap.id),
            None => true,
        })
        .collect();
    snaps.sort_by(|left, right| {
        left.added_at
            .cmp(&right.added_at)
            .then_with(|| left.id.to_string().cmp(&right.id.to_string()))
    });
    snaps
}

fn passes_filter(snap: &TorrentSnapshot, filter: &str) -> bool {
    let state = qb_state(snap);
    match filter {
        "" | "all" => true,
        "downloading" => matches!(
            state,
            "downloading" | "metaDL" | "queuedDL" | "checkingDL" | "forcedDL"
        ),
        "seeding" => matches!(state, "uploading" | "queuedUP" | "checkingUP"),
        "completed" => matches!(state, "uploading" | "pausedUP" | "queuedUP" | "checkingUP"),
        "paused" | "stopped" => state.starts_with("paused"),
        "resumed" => !state.starts_with("paused"),
        "active" => snap.download_rate > 0 || snap.upload_rate > 0,
        "inactive" => snap.download_rate == 0 && snap.upload_rate == 0,
        "errored" => state == "error",
        "stalled" | "stalled_downloading" | "stalled_uploading" => {
            snap.download_rate == 0
                && snap.upload_rate == 0
                && matches!(state, "downloading" | "uploading" | "metaDL")
        }
        _ => true,
    }
}

pub(crate) fn qb_state(snap: &TorrentSnapshot) -> &'static str {
    let complete = snap.completed_at.is_some() || (snap.size > 0 && snap.left == 0);
    match &snap.state {
        TorrentState::Checking => {
            if complete {
                "checkingUP"
            } else {
                "checkingDL"
            }
        }
        TorrentState::Metadata => "metaDL",
        TorrentState::Downloading => "downloading",
        TorrentState::Seeding => "uploading",
        TorrentState::Paused => {
            if complete {
                "pausedUP"
            } else {
                "pausedDL"
            }
        }
        TorrentState::Queued => {
            if complete {
                "queuedUP"
            } else {
                "queuedDL"
            }
        }
        TorrentState::Moving => "moving",
        TorrentState::Error(_) => "error",
    }
}

fn info_json(snap: &TorrentSnapshot) -> Value {
    json!({
        "hash": snap.id.to_string(),
        "name": snap.name,
        "state": qb_state(snap),
        "progress": snap.progress,
        "dlspeed": snap.download_rate,
        "upspeed": snap.upload_rate,
        "size": snap.size,
        "amount_left": snap.left,
        "completed": snap.size.saturating_sub(snap.left),
        "downloaded": snap.downloaded,
        "uploaded": snap.uploaded,
        "ratio": snap.ratio,
        "save_path": path_string(&snap.save_path),
        "content_path": content_path(snap),
        "category": snap.category,
        "tags": snap.tags.join(", "),
        "seq_dl": snap.sequential,
        "force_start": snap.force_start,
        "completion_on": snap.completed_at.unwrap_or(-1),
        "added_on": snap.added_at,
        "num_leechs": snap.peers.saturating_sub(snap.seeds),
        "num_seeds": snap.seeds,
        "eta": eta(snap),
        "ratio_limit": qb_ratio(snap.ratio_limit),
        "seeding_time_limit": qb_seed_minutes(snap.seeding_time_limit),
    })
}

fn file_json(info: &FileInfo) -> Value {
    json!({
        "index": info.index,
        "name": path_string(&info.path),
        "size": info.length,
        "progress": info.progress,
        "priority": to_qb_prio(info.priority),
        "is_seed": info.progress >= 1.0,
        "availability": 0.0,
    })
}

fn content_path(snap: &TorrentSnapshot) -> String {
    path_string(&snap.save_path)
}

fn eta(snap: &TorrentSnapshot) -> i64 {
    if snap.left == 0 || snap.download_rate == 0 {
        ETA_INFINITY
    } else {
        (snap.left / snap.download_rate) as i64
    }
}

fn qb_ratio(limit: Option<f64>) -> f64 {
    match limit {
        None => -2.0,
        Some(value) if value <= 0.0 => -1.0,
        Some(value) => value,
    }
}

fn qb_seed_minutes(limit: Option<u64>) -> i64 {
    match limit {
        None => -2,
        Some(0) => -1,
        Some(minutes) => minutes as i64,
    }
}

fn one_snapshot(state: &AppState, hash: Option<&str>) -> Option<TorrentSnapshot> {
    let id = parse_id(hash?).ok()?;
    state.session.snapshot(id)
}

struct AddFields {
    urls: String,
    torrents: Vec<Vec<u8>>,
    savepath: Option<String>,
    category: Option<String>,
    tags: Vec<String>,
    paused: Option<bool>,
    skip_checking: bool,
    sequential: bool,
    ratio_limit: Option<Option<f64>>,
    seeding_time_limit: Option<Option<u64>>,
    auto_tmm: Option<bool>,
    download_limit: Option<u64>,
    upload_limit: Option<u64>,
}

impl AddFields {
    fn from_map(map: &HashMap<String, String>) -> Result<Self, ()> {
        let paused_flag = map.get("paused").and_then(|value| parse_bool(value));
        let stopped_flag = map.get("stopped").and_then(|value| parse_bool(value));
        let paused = match (paused_flag, stopped_flag) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (None, None) => None,
            _ => Some(false),
        };
        let ratio_limit = match map.get("ratioLimit") {
            Some(value) => Some(parse_ratio_limit(value).ok_or(())?),
            None => None,
        };
        let seeding_time_limit = match map.get("seedingTimeLimit") {
            Some(value) => Some(parse_seed_minutes(value).ok_or(())?),
            None => None,
        };
        let download_limit = match map.get("dlLimit") {
            Some(value) => Some(parse_byte_limit(value).ok_or(())?),
            None => None,
        };
        let upload_limit = match map.get("upLimit") {
            Some(value) => Some(parse_byte_limit(value).ok_or(())?),
            None => None,
        };
        Ok(Self {
            urls: map.get("urls").cloned().unwrap_or_default(),
            torrents: Vec::new(),
            savepath: nonempty(map.get("savepath").cloned()),
            category: nonempty(map.get("category").cloned()),
            tags: map
                .get("tags")
                .map(|value| split_tags(value))
                .unwrap_or_default(),
            paused,
            skip_checking: map
                .get("skip_checking")
                .and_then(|value| parse_bool(value))
                .unwrap_or(false),
            sequential: map
                .get("sequentialDownload")
                .and_then(|value| parse_bool(value))
                .unwrap_or(false),
            ratio_limit,
            seeding_time_limit,
            auto_tmm: map.get("autoTMM").and_then(|value| parse_bool(value)),
            download_limit,
            upload_limit,
        })
    }
}

fn apply_add_fields(mut options: AddTorrentOptions, fields: &AddFields) -> AddTorrentOptions {
    if let Some(path) = &fields.savepath {
        options = options.save_path(path);
    }
    if let Some(category) = &fields.category {
        options = options.category(category);
    }
    if !fields.tags.is_empty() {
        options = options.tags(fields.tags.clone());
    }
    if let Some(paused) = fields.paused {
        options = options.paused(paused);
    }
    if fields.skip_checking {
        options = options.skip_checking(true);
    }
    if fields.sequential {
        options = options.sequential(true);
    }
    if let Some(Some(limit)) = fields.ratio_limit {
        options = options.ratio_limit(limit);
    }
    if let Some(Some(minutes)) = fields.seeding_time_limit {
        options = options.seeding_time_limit(minutes);
    }
    if let Some(enabled) = fields.auto_tmm {
        options = options.auto_tmm(enabled);
    }
    if let Some(limit) = fields.download_limit {
        options = options.download_limit(limit);
    }
    if let Some(limit) = fields.upload_limit {
        options = options.upload_limit(limit);
    }
    options
}

async fn read_add_multipart(req: Request, state: &AppState) -> Result<AddFields, ()> {
    let mut multipart = Multipart::from_request(req, state).await.map_err(|_| ())?;
    let mut textual: HashMap<String, String> = HashMap::new();
    let mut torrents = Vec::new();
    while let Some(field) = multipart.next_field().await.map_err(|_| ())? {
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(str::to_string);
        let bytes = field.bytes().await.map_err(|_| ())?;
        if filename.is_some() || name == "torrents" || name == "torrent" {
            if !bytes.is_empty() {
                torrents.push(bytes.to_vec());
            }
            continue;
        }
        let Ok(text) = String::from_utf8(bytes.to_vec()) else {
            continue;
        };
        textual.insert(name, text);
    }
    let mut fields = AddFields::from_map(&textual)?;
    fields.torrents = torrents;
    Ok(fields)
}

async fn fetch_torrent(url: &str) -> Result<Vec<u8>, ()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| ())?;
    let response = client.get(url).send().await.map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let bytes = response.bytes().await.map_err(|_| ())?;
    Ok(bytes.to_vec())
}

async fn read_form(req: Request, state: &AppState) -> Result<HashMap<String, String>, ()> {
    let mut map = HashMap::new();
    if let Some(query) = req.uri().query() {
        if let Ok(parsed) = serde_urlencoded::from_str::<HashMap<String, String>>(query) {
            map.extend(parsed);
        }
    }
    let bytes = Bytes::from_request(req, state).await.map_err(|_| ())?;
    if !bytes.is_empty() {
        if let Ok(parsed) = serde_urlencoded::from_bytes::<HashMap<String, String>>(&bytes) {
            map.extend(parsed);
        }
    }
    Ok(map)
}

fn content_type_of(req: &Request) -> String {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn parse_id(hash: &str) -> Result<TorrentId, ()> {
    let hash = hash.trim();
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(());
    }
    let mut bytes = [0u8; 20];
    for (index, chunk) in hash.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(chunk).map_err(|_| ())?;
        bytes[index] = u8::from_str_radix(text, 16).map_err(|_| ())?;
    }
    Ok(TorrentId::new(bytes))
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_byte_limit(value: &str) -> Option<u64> {
    let value: i64 = value.trim().parse().ok()?;
    if value < 0 {
        Some(0)
    } else {
        Some(value as u64)
    }
}

fn parse_ratio_limit(value: &str) -> Option<Option<f64>> {
    let value: f64 = value.trim().parse().ok()?;
    if !value.is_finite() {
        return None;
    }
    if (value + 2.0).abs() < 1e-9 {
        return Some(None);
    }
    if (value + 1.0).abs() < 1e-9 {
        return Some(Some(0.0));
    }
    if value < 0.0 {
        return None;
    }
    Some(Some(value))
}

fn parse_seed_minutes(value: &str) -> Option<Option<u64>> {
    let value: i64 = value.trim().parse().ok()?;
    match value {
        -2 => Some(None),
        -1 => Some(Some(0)),
        minutes if minutes >= 0 => Some(Some(minutes as u64)),
        _ => None,
    }
}

fn json_limit(value: &Value) -> Option<u64> {
    let number = if let Some(number) = value.as_f64() {
        number
    } else if let Some(text) = value.as_str() {
        text.trim().parse().ok()?
    } else if let Some(number) = value.as_i64() {
        number as f64
    } else {
        value.as_u64()? as f64
    };
    if !number.is_finite() {
        return None;
    }
    if number < 0.0 {
        Some(0)
    } else {
        Some(number as u64)
    }
}

fn split_urls(urls: &str) -> Vec<String> {
    urls.split(['\n', '\r'])
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
        .collect()
}

fn split_tags(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

fn split_list(text: &str) -> Vec<String> {
    text.split(['|', '\n', '\r'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn from_qb_prio(priority: i64) -> FilePriority {
    if priority <= 0 {
        FilePriority::Skip
    } else if priority >= 6 {
        FilePriority::High
    } else {
        FilePriority::Normal
    }
}

fn to_qb_prio(priority: FilePriority) -> i64 {
    match priority {
        FilePriority::Skip => 0,
        FilePriority::Low | FilePriority::Normal => 1,
        FilePriority::High => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn snap(
        state: TorrentState,
        size: u64,
        left: u64,
        completed_at: Option<i64>,
    ) -> TorrentSnapshot {
        TorrentSnapshot {
            id: TorrentId::new([0; 20]),
            name: "episode".into(),
            info_hash: [0; 20],
            state,
            error: None,
            progress: if size == 0 {
                0.0
            } else {
                (size - left) as f64 / size as f64
            },
            downloaded: size - left,
            uploaded: 0,
            left,
            size,
            download_rate: 0,
            upload_rate: 0,
            peers: 0,
            seeds: 0,
            save_path: PathBuf::from("/data"),
            torrent_path: PathBuf::new(),
            category: String::new(),
            tags: Vec::new(),
            sequential: false,
            added_at: 0,
            completed_at,
            piece_count: 1,
            pieces_have: 0,
            ratio: 0.0,
            queue_position: 0,
            force_start: false,
            ratio_limit: None,
            seeding_time_limit: None,
            piece_length: 16 * 1024,
        }
    }

    #[test]
    fn state_map_matches_spec_24() {
        assert_eq!(
            qb_state(&snap(TorrentState::Checking, 10, 10, None)),
            "checkingDL"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Checking, 10, 0, None)),
            "checkingUP"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Metadata, 0, 0, None)),
            "metaDL"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Downloading, 10, 4, None)),
            "downloading"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Seeding, 10, 0, Some(1))),
            "uploading"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Paused, 10, 4, None)),
            "pausedDL"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Paused, 10, 0, Some(1))),
            "pausedUP"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Queued, 10, 4, None)),
            "queuedDL"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Queued, 10, 0, Some(1))),
            "queuedUP"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Moving, 10, 0, Some(1))),
            "moving"
        );
        assert_eq!(
            qb_state(&snap(TorrentState::Error("disk".into()), 10, 1, None)),
            "error"
        );
    }
}
