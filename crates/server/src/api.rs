//! Native `/api/v1` JSON API. Every handler calls [`Session`].

use std::convert::Infallible;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use axum::extract::{FromRequest, Multipart, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use bit_rev::file;
use bit_rev::identity::CLIENT_VERSION;
use bit_rev::magnet::Magnet;
use bit_rev::priority::{FileInfo, FilePriority};
use bit_rev::session::{
    AddTorrentOptions, ControlError, SessionEvent, TorrentId, TorrentSnapshot, TorrentState,
    TransferStats,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::auth::{login, logout};
use crate::AppState;

const MAX_SSE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/v1/login", post(login))
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/session", get(session_info))
        .route("/api/v1/settings", get(get_settings).patch(patch_settings))
        .route("/api/v1/torrents", get(list_torrents).post(add_torrent))
        .route(
            "/api/v1/torrents/{hash}",
            get(get_torrent).delete(delete_torrent),
        )
        .route("/api/v1/torrents/{hash}/pause", post(pause_torrent))
        .route("/api/v1/torrents/{hash}/resume", post(resume_torrent))
        .route("/api/v1/torrents/{hash}/recheck", post(recheck_torrent))
        .route(
            "/api/v1/torrents/{hash}/files",
            get(list_files).patch(patch_files),
        )
        .route(
            "/api/v1/categories",
            get(list_categories).post(create_category),
        )
        .route("/api/v1/events", get(events))
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

fn control(err: ControlError) -> ApiError {
    match err {
        ControlError::NotFound(_) | ControlError::CategoryNotFound(_) => {
            ApiError::not_found(err.to_string())
        }
        ControlError::NoSuchFile { .. } => ApiError::bad_request(err.to_string()),
        ControlError::NoMetadata(_) | ControlError::CategoryExists(_) => {
            ApiError::conflict(err.to_string())
        }
        ControlError::CategoryPersist(_) | ControlError::MoveFailed { .. } => {
            ApiError::internal(err.to_string())
        }
    }
}

fn parse_id(hash: &str) -> Result<TorrentId, ApiError> {
    if hash.len() != 40 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request("hash must be 40 hex characters"));
    }
    let mut bytes = [0u8; 20];
    for (index, chunk) in hash.as_bytes().chunks(2).enumerate() {
        let text = std::str::from_utf8(chunk).expect("ascii hex");
        bytes[index] = u8::from_str_radix(text, 16).expect("hex digit");
    }
    Ok(TorrentId::new(bytes))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum StateBody {
    Checking,
    Metadata,
    Downloading,
    Seeding,
    Paused,
    Queued,
    Moving,
    Error(String),
}

impl From<TorrentState> for StateBody {
    fn from(state: TorrentState) -> Self {
        match state {
            TorrentState::Checking => Self::Checking,
            TorrentState::Metadata => Self::Metadata,
            TorrentState::Downloading => Self::Downloading,
            TorrentState::Seeding => Self::Seeding,
            TorrentState::Paused => Self::Paused,
            TorrentState::Queued => Self::Queued,
            TorrentState::Moving => Self::Moving,
            TorrentState::Error(message) => Self::Error(message),
        }
    }
}

#[derive(Serialize)]
struct SnapshotBody {
    id: String,
    name: String,
    info_hash: String,
    state: StateBody,
    error: Option<String>,
    progress: f64,
    downloaded: u64,
    uploaded: u64,
    left: u64,
    size: u64,
    download_rate: u64,
    upload_rate: u64,
    peers: u32,
    seeds: u32,
    save_path: String,
    torrent_path: String,
    category: String,
    tags: Vec<String>,
    sequential: bool,
    added_at: i64,
    completed_at: Option<i64>,
    piece_count: u32,
    pieces_have: u32,
    ratio: f64,
    queue_position: i64,
    force_start: bool,
}

impl From<TorrentSnapshot> for SnapshotBody {
    fn from(snap: TorrentSnapshot) -> Self {
        Self {
            id: snap.id.to_string(),
            name: snap.name,
            info_hash: hex(&snap.info_hash),
            state: snap.state.into(),
            error: snap.error,
            progress: snap.progress,
            downloaded: snap.downloaded,
            uploaded: snap.uploaded,
            left: snap.left,
            size: snap.size,
            download_rate: snap.download_rate,
            upload_rate: snap.upload_rate,
            peers: snap.peers,
            seeds: snap.seeds,
            save_path: path_string(&snap.save_path),
            torrent_path: path_string(&snap.torrent_path),
            category: snap.category,
            tags: snap.tags,
            sequential: snap.sequential,
            added_at: snap.added_at,
            completed_at: snap.completed_at,
            piece_count: snap.piece_count,
            pieces_have: snap.pieces_have,
            ratio: snap.ratio,
            queue_position: snap.queue_position,
            force_start: snap.force_start,
        }
    }
}

#[derive(Serialize)]
struct IpFilterBody {
    ranges: usize,
    hits: u64,
    loaded_at: Option<u64>,
}

#[derive(Serialize)]
struct SessionBody {
    downloaded: u64,
    uploaded: u64,
    download_rate: u64,
    upload_rate: u64,
    torrents: usize,
    download_limit: u64,
    upload_limit: u64,
    alt_download_limit: u64,
    alt_upload_limit: u64,
    alt_mode: bool,
    port_open: Option<bool>,
    ip_filter: IpFilterBody,
    listen_port: u16,
    version: &'static str,
}

impl SessionBody {
    fn from_stats(stats: TransferStats, listen_port: u16) -> Self {
        let loaded_at = stats.ip_filter.loaded_at.and_then(|time| {
            time.duration_since(UNIX_EPOCH)
                .ok()
                .map(|elapsed| elapsed.as_secs())
        });
        Self {
            downloaded: stats.downloaded,
            uploaded: stats.uploaded,
            download_rate: stats.download_rate,
            upload_rate: stats.upload_rate,
            torrents: stats.torrents,
            download_limit: stats.download_limit,
            upload_limit: stats.upload_limit,
            alt_download_limit: stats.alt_download_limit,
            alt_upload_limit: stats.alt_upload_limit,
            alt_mode: stats.alt_mode,
            port_open: stats.port_open,
            ip_filter: IpFilterBody {
                ranges: stats.ip_filter.ranges,
                hits: stats.ip_filter.hits,
                loaded_at,
            },
            listen_port,
            version: CLIENT_VERSION,
        }
    }
}

#[derive(Serialize)]
struct FileBody {
    index: usize,
    path: String,
    length: u64,
    progress: f64,
    priority: &'static str,
}

impl From<FileInfo> for FileBody {
    fn from(info: FileInfo) -> Self {
        Self {
            index: info.index,
            path: path_string(&info.path),
            length: info.length,
            progress: info.progress,
            priority: priority_name(info.priority),
        }
    }
}

fn priority_name(priority: FilePriority) -> &'static str {
    match priority {
        FilePriority::Skip => "skip",
        FilePriority::Low => "low",
        FilePriority::Normal => "normal",
        FilePriority::High => "high",
    }
}

fn parse_priority(text: &str) -> Result<FilePriority, ApiError> {
    match text.trim().to_ascii_lowercase().as_str() {
        "skip" => Ok(FilePriority::Skip),
        "low" => Ok(FilePriority::Low),
        "normal" => Ok(FilePriority::Normal),
        "high" => Ok(FilePriority::High),
        _ => Err(ApiError::bad_request(format!("unknown priority {text}"))),
    }
}

#[derive(Serialize)]
struct CategoryBody {
    name: String,
    save_path: Option<String>,
}

#[derive(Deserialize)]
struct CategoryInput {
    name: String,
    #[serde(default)]
    save_path: Option<String>,
}

#[derive(Deserialize, Default)]
struct DeleteQuery {
    #[serde(default)]
    delete_files: Option<String>,
}

impl DeleteQuery {
    fn delete_files(&self) -> bool {
        self.delete_files.as_deref().is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    }
}

#[derive(Deserialize)]
struct FilePatch {
    index: usize,
    priority: String,
}

#[derive(Deserialize)]
struct PatchFiles {
    #[serde(default)]
    priorities: Option<Vec<String>>,
    #[serde(default)]
    files: Option<Vec<FilePatch>>,
}

#[derive(Deserialize)]
struct AddBody {
    #[serde(default)]
    magnet: Option<String>,
    #[serde(default)]
    save_path: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    paused: Option<bool>,
    #[serde(default)]
    sequential: Option<bool>,
}

struct AddInput {
    magnet: Option<String>,
    torrent: Option<Vec<u8>>,
    save_path: Option<String>,
    category: Option<String>,
    tags: Option<Vec<String>>,
    paused: Option<bool>,
    sequential: Option<bool>,
}

impl AddInput {
    fn into_options(self) -> Result<AddTorrentOptions, ApiError> {
        let mut options = if let Some(bytes) = self.torrent {
            let meta =
                file::from_bytes(&bytes).map_err(|err| ApiError::bad_request(err.to_string()))?;
            AddTorrentOptions::from(meta)
        } else if let Some(magnet) = nonempty(self.magnet) {
            let magnet =
                Magnet::parse(&magnet).map_err(|err| ApiError::bad_request(err.to_string()))?;
            AddTorrentOptions::from_magnet(&magnet)
        } else {
            return Err(ApiError::bad_request("magnet or .torrent file is required"));
        };
        if let Some(path) = nonempty(self.save_path) {
            options = options.save_path(path);
        }
        if let Some(category) = nonempty(self.category) {
            options = options.category(category);
        }
        if let Some(tags) = self.tags {
            options = options.tags(tags);
        }
        if let Some(paused) = self.paused {
            options = options.paused(paused);
        }
        if let Some(sequential) = self.sequential {
            options = options.sequential(sequential);
        }
        Ok(options)
    }
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

fn optional_path(value: Option<String>) -> Option<PathBuf> {
    nonempty(value).map(PathBuf::from)
}

fn flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        "" => None,
        _ => None,
    }
}

fn snapshot_of(state: &AppState, id: TorrentId) -> Result<SnapshotBody, ApiError> {
    state
        .session
        .snapshot(id)
        .map(SnapshotBody::from)
        .ok_or_else(|| ApiError::not_found(format!("torrent {id} not found")))
}

fn files_of(state: &AppState, id: TorrentId) -> Result<Vec<FileBody>, ApiError> {
    match state.session.files(id) {
        Ok(files) => Ok(files.into_iter().map(FileBody::from).collect()),
        Err(ControlError::NoMetadata(_)) => Ok(Vec::new()),
        Err(err) => Err(control(err)),
    }
}

async fn session_info(State(state): State<AppState>) -> Json<SessionBody> {
    Json(SessionBody::from_stats(
        state.session.transfer_stats(),
        state.session.listen_port(),
    ))
}

#[derive(Serialize)]
struct SettingsBody {
    download_dir: String,
    listen_port: u16,
    max_peers_per_torrent: usize,
    max_connections: usize,
    dht: bool,
    pex: bool,
    lpd: bool,
    nat: bool,
    webseed: bool,
    upload_limit: u64,
    download_limit: u64,
    alt_upload_limit: u64,
    alt_download_limit: u64,
    alt_mode: bool,
    seed_ratio_limit: f64,
    seed_time_limit: u64,
    max_active_downloads: usize,
    max_active_uploads: usize,
    max_active: usize,
    server_host: String,
    server_port: u16,
    server_username: String,
    qbittorrent_compat: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsPatch {
    #[serde(default)]
    download_limit: Option<u64>,
    #[serde(default)]
    upload_limit: Option<u64>,
    #[serde(default)]
    alt_download_limit: Option<u64>,
    #[serde(default)]
    alt_upload_limit: Option<u64>,
    #[serde(default)]
    alt_mode: Option<bool>,
}

impl SettingsPatch {
    fn is_empty(&self) -> bool {
        self.download_limit.is_none()
            && self.upload_limit.is_none()
            && self.alt_download_limit.is_none()
            && self.alt_upload_limit.is_none()
            && self.alt_mode.is_none()
    }
}

fn settings_body(state: &AppState) -> SettingsBody {
    let options = state.session.options();
    let (download_limit, upload_limit) = state.session.normal_rate_limits();
    let stats = state.session.transfer_stats();
    SettingsBody {
        download_dir: path_string(&options.download_dir),
        listen_port: state.session.listen_port(),
        max_peers_per_torrent: options.max_peers_per_torrent,
        max_connections: options.max_peers_global,
        dht: options.dht.enabled,
        pex: options.pex,
        lpd: options.lpd,
        nat: options.nat.enabled,
        webseed: options.webseed,
        upload_limit,
        download_limit,
        alt_upload_limit: stats.alt_upload_limit,
        alt_download_limit: stats.alt_download_limit,
        alt_mode: stats.alt_mode,
        seed_ratio_limit: options.seed_ratio_limit,
        seed_time_limit: options.seed_time_limit,
        max_active_downloads: options.max_active_downloads,
        max_active_uploads: options.max_active_uploads,
        max_active: options.max_active,
        server_host: state.config.host.clone(),
        server_port: state.config.port,
        server_username: state.config.username.clone(),
        qbittorrent_compat: state.config.qbittorrent_compat,
    }
}

async fn get_settings(State(state): State<AppState>) -> Json<SettingsBody> {
    Json(settings_body(&state))
}

async fn patch_settings(
    State(state): State<AppState>,
    Json(body): Json<SettingsPatch>,
) -> Result<Json<SettingsBody>, ApiError> {
    if body.is_empty() {
        return Err(ApiError::bad_request("no settings to apply"));
    }
    if body.download_limit.is_some() || body.upload_limit.is_some() {
        let (mut download, mut upload) = state.session.normal_rate_limits();
        if let Some(limit) = body.download_limit {
            download = limit;
        }
        if let Some(limit) = body.upload_limit {
            upload = limit;
        }
        state.session.set_rate_limits(download, upload);
    }
    if body.alt_download_limit.is_some() || body.alt_upload_limit.is_some() {
        let stats = state.session.transfer_stats();
        let download = body.alt_download_limit.unwrap_or(stats.alt_download_limit);
        let upload = body.alt_upload_limit.unwrap_or(stats.alt_upload_limit);
        state.session.set_alt_limits(download, upload);
    }
    if let Some(on) = body.alt_mode {
        state.session.set_alt_mode(on);
    }
    Ok(Json(settings_body(&state)))
}

async fn list_torrents(State(state): State<AppState>) -> Json<Vec<SnapshotBody>> {
    Json(
        state
            .session
            .list()
            .into_iter()
            .map(SnapshotBody::from)
            .collect(),
    )
}

async fn get_torrent(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<SnapshotBody>, ApiError> {
    let id = parse_id(&hash)?;
    Ok(Json(snapshot_of(&state, id)?))
}

async fn add_torrent(
    State(state): State<AppState>,
    req: Request,
) -> Result<(StatusCode, Json<SnapshotBody>), ApiError> {
    let input = read_add(req, &state).await?;
    let options = input.into_options()?;
    let added = state
        .session
        .add_torrent(options)
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;
    Ok((StatusCode::CREATED, Json(snapshot_of(&state, added.id)?)))
}

async fn read_add(req: Request, state: &AppState) -> Result<AddInput, ApiError> {
    let content_type = req
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    if content_type.starts_with("multipart/") {
        let multipart = Multipart::from_request(req, state)
            .await
            .map_err(|err| ApiError::bad_request(err.body_text()))?;
        return read_multipart(multipart).await;
    }
    if content_type.starts_with("application/json") || content_type.is_empty() {
        let Json(body) = Json::<AddBody>::from_request(req, state)
            .await
            .map_err(|err| ApiError::bad_request(err.body_text()))?;
        return Ok(AddInput {
            magnet: body.magnet,
            torrent: None,
            save_path: body.save_path,
            category: body.category,
            tags: body.tags,
            paused: body.paused,
            sequential: body.sequential,
        });
    }
    Err(ApiError::new(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "expected application/json or multipart/form-data",
    ))
}

async fn read_multipart(mut multipart: Multipart) -> Result<AddInput, ApiError> {
    let mut input = AddInput {
        magnet: None,
        torrent: None,
        save_path: None,
        category: None,
        tags: None,
        paused: None,
        sequential: None,
    };
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| ApiError::bad_request(err.to_string()))?
    {
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(str::to_string);
        let bytes = field
            .bytes()
            .await
            .map_err(|err| ApiError::bad_request(err.to_string()))?;
        if filename.is_some() || matches!(name.as_str(), "torrent" | "torrents" | "file") {
            if input.torrent.is_none() {
                input.torrent = Some(bytes.to_vec());
            }
            continue;
        }
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| ApiError::bad_request(format!("field {name} is not utf-8")))?;
        match name.as_str() {
            "magnet" => input.magnet = Some(text),
            "save_path" => input.save_path = Some(text),
            "category" => input.category = Some(text),
            "tags" => {
                let mut tags = input.tags.take().unwrap_or_default();
                tags.extend(split_tags(&text));
                input.tags = Some(tags);
            }
            "paused" => input.paused = flag(&text).or(input.paused),
            "sequential" => input.sequential = flag(&text).or(input.sequential),
            _ => {}
        }
    }
    Ok(input)
}

fn split_tags(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

async fn pause_torrent(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<SnapshotBody>, ApiError> {
    let id = parse_id(&hash)?;
    state.session.pause(id).map_err(control)?;
    Ok(Json(snapshot_of(&state, id)?))
}

async fn resume_torrent(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<SnapshotBody>, ApiError> {
    let id = parse_id(&hash)?;
    state.session.resume(id).map_err(control)?;
    Ok(Json(snapshot_of(&state, id)?))
}

async fn recheck_torrent(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<SnapshotBody>, ApiError> {
    let id = parse_id(&hash)?;
    state.session.recheck(id).await.map_err(control)?;
    Ok(Json(snapshot_of(&state, id)?))
}

async fn delete_torrent(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> Result<Json<Value>, ApiError> {
    let id = parse_id(&hash)?;
    state
        .session
        .remove_torrent(id, query.delete_files())
        .map_err(control)?;
    Ok(Json(json!({"ok": true})))
}

async fn list_files(
    State(state): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Json<Vec<FileBody>>, ApiError> {
    let id = parse_id(&hash)?;
    Ok(Json(files_of(&state, id)?))
}

async fn patch_files(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    Json(body): Json<PatchFiles>,
) -> Result<Json<Vec<FileBody>>, ApiError> {
    let id = parse_id(&hash)?;
    if body.priorities.is_none() && body.files.is_none() {
        return Err(ApiError::bad_request("priorities are required"));
    }
    if let Some(priorities) = body.priorities {
        let mut parsed = Vec::with_capacity(priorities.len());
        for priority in priorities {
            parsed.push(parse_priority(&priority)?);
        }
        state
            .session
            .set_file_priorities(id, parsed)
            .map_err(control)?;
    }
    if let Some(files) = body.files {
        for file in files {
            let priority = parse_priority(&file.priority)?;
            state
                .session
                .set_file_priority(id, file.index, priority)
                .map_err(control)?;
        }
    }
    Ok(Json(files_of(&state, id)?))
}

async fn list_categories(State(state): State<AppState>) -> Json<Vec<CategoryBody>> {
    Json(
        state
            .session
            .categories()
            .into_iter()
            .map(|category| CategoryBody {
                name: category.name,
                save_path: category.save_path.map(|path| path_string(&path)),
            })
            .collect(),
    )
}

async fn create_category(
    State(state): State<AppState>,
    Json(body): Json<CategoryInput>,
) -> Result<(StatusCode, Json<CategoryBody>), ApiError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("category name is required"));
    }
    let save_path = optional_path(body.save_path);
    state
        .session
        .create_category(name, save_path.clone())
        .map_err(control)?;
    Ok((
        StatusCode::CREATED,
        Json(CategoryBody {
            name: name.to_string(),
            save_path: save_path.map(|path| path_string(&path)),
        }),
    ))
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventBody {
    Added {
        id: String,
    },
    Removed {
        id: String,
    },
    StateChanged {
        id: String,
        state: StateBody,
    },
    Progress {
        id: String,
        snapshot: Box<SnapshotBody>,
    },
    Completed {
        id: String,
    },
    Error {
        id: String,
        message: String,
    },
}

impl EventBody {
    fn name(&self) -> &'static str {
        match self {
            Self::Added { .. } => "added",
            Self::Removed { .. } => "removed",
            Self::StateChanged { .. } => "state_changed",
            Self::Progress { .. } => "progress",
            Self::Completed { .. } => "completed",
            Self::Error { .. } => "error",
        }
    }
}

impl From<SessionEvent> for EventBody {
    fn from(event: SessionEvent) -> Self {
        match event {
            SessionEvent::Added { id } => Self::Added { id: id.to_string() },
            SessionEvent::Removed { id } => Self::Removed { id: id.to_string() },
            SessionEvent::StateChanged { id, state } => Self::StateChanged {
                id: id.to_string(),
                state: state.into(),
            },
            SessionEvent::Progress { id, snapshot } => Self::Progress {
                id: id.to_string(),
                snapshot: Box::new(SnapshotBody::from(*snapshot)),
            },
            SessionEvent::Completed { id } => Self::Completed { id: id.to_string() },
            SessionEvent::Error { id, message } => Self::Error {
                id: id.to_string(),
                message,
            },
        }
    }
}

fn sse_event(event: SessionEvent) -> Event {
    let body = EventBody::from(event);
    let name = body.name();
    let data = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    Event::default().event(name).data(data)
}

/// Stream [`SessionEvent`]s. The handler holds only a broadcast receiver.
/// Dropping the response drops that receiver. `Session` never awaits it, so a
/// disconnected client cannot stall the engine.
async fn events(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>> + Send> {
    let rx = state.session.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(event) => Some(Ok(sse_event(event))),
        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
            tracing::debug!(skipped, "sse subscriber lagged");
            None
        }
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(MAX_SSE_INTERVAL)
            .text("keep-alive"),
    )
}
