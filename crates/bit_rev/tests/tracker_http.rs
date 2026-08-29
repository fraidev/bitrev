use std::{
    collections::HashMap,
    io::ErrorKind,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use bit_rev::{
    file::{Info, TorrentFile, TorrentMeta},
    identity,
    peer_connection::{PieceWorkState, TorrentDownloadedState},
    peer_state::PeerStates,
    session::{DownloadState, PieceWork},
    tracker::{
        self, announce, build_http_client, http_client, run_http_announce_loop,
        HttpAnnounceContext, TrackerError,
    },
};
use serde::Serialize;
use serde_bencode::ser;
use serde_bytes::ByteBuf;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Semaphore},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
struct RecordedRequest {
    path: String,
    query: String,
    headers: HashMap<String, String>,
}

impl RecordedRequest {
    fn query_param(&self, key: &str) -> Option<&str> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then_some(v)
        })
    }
}

#[derive(Clone)]
struct MockResponse {
    status: u16,
    body: Vec<u8>,
}

#[derive(Serialize)]
struct TestAnnounce {
    interval: i64,
    #[serde(rename = "min interval", skip_serializing_if = "Option::is_none")]
    min_interval: Option<i64>,
    #[serde(rename = "tracker id", skip_serializing_if = "Option::is_none")]
    tracker_id: Option<ByteBuf>,
    #[serde(rename = "warning message", skip_serializing_if = "Option::is_none")]
    warning_message: Option<String>,
    peers: ByteBuf,
}

fn compact_peer(ip: [u8; 4], port: u16) -> Vec<u8> {
    let mut buf = ip.to_vec();
    buf.extend_from_slice(&port.to_be_bytes());
    buf
}

fn ok_announce(interval: i64, min_interval: Option<i64>, tracker_id: Option<&[u8]>) -> Vec<u8> {
    ser::to_bytes(&TestAnnounce {
        interval,
        min_interval,
        tracker_id: tracker_id.map(ByteBuf::from),
        warning_message: None,
        peers: ByteBuf::from(compact_peer([127, 0, 0, 1], 51413)),
    })
    .unwrap()
}

fn failure_announce(reason: &str) -> Vec<u8> {
    format!("d14:failure reason{}:{reason}e", reason.len()).into_bytes()
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        403 => "Forbidden",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

async fn read_http_request(stream: &mut TcpStream) -> std::io::Result<RecordedRequest> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "client closed before request completed",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "request too large",
            ));
        }
    }

    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    Ok(RecordedRequest {
        path: path.to_string(),
        query: query.to_string(),
        headers,
    })
}

async fn write_http_response(
    stream: &mut TcpStream,
    response: &MockResponse,
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/plain\r\n\r\n",
        response.status,
        status_reason(response.status),
        response.body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.shutdown().await
}

async fn spawn_scripted_tracker(
    responses: Vec<MockResponse>,
) -> (
    String,
    mpsc::UnboundedReceiver<RecordedRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    let idx = Arc::new(AtomicUsize::new(0));
    let responses = Arc::new(responses);

    let handle = tokio::spawn(async move {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let request = match read_http_request(&mut stream).await {
                Ok(request) => request,
                Err(_) => continue,
            };
            let i = idx.fetch_add(1, Ordering::SeqCst);
            let response = responses
                .get(i)
                .cloned()
                .or_else(|| responses.last().cloned())
                .unwrap_or(MockResponse {
                    status: 200,
                    body: ok_announce(1800, None, None),
                });
            let _ = write_http_response(&mut stream, &response).await;
            let _ = tx.send(request);
        }
    });

    (format!("http://{addr}/announce"), rx, handle)
}

fn test_meta(announce: String) -> TorrentMeta {
    TorrentMeta {
        torrent_file: TorrentFile {
            info: Info {
                name: "test".into(),
                pieces: ByteBuf::from(vec![0u8; 20]),
                piece_length: 16384,
                md5sum: None,
                length: Some(16384),
                files: None,
                private: None,
                path: None,
                root_hash: None,
            },
            announce: Some(announce),
            nodes: None,
            encoding: None,
            httpseeds: None,
            announce_list: None,
            creation_date: None,
            comment: None,
            created_by: None,
        },
        info_hash: [1u8; 20],
        piece_hashes: vec![[1u8; 20]],
    }
}

fn incomplete_download() -> Arc<TorrentDownloadedState> {
    Arc::new(TorrentDownloadedState {
        semaphore: Semaphore::new(1),
        pieces: vec![PieceWorkState {
            piece_work: PieceWork {
                index: 0,
                length: 16384,
                hash: [1u8; 20],
            },
            chuncks: Mutex::new(vec![]),
            downloaded: AtomicBool::new(false),
            reserved: Mutex::new(None),
        }],
    })
}

fn announce_ctx(url: String, download: Arc<TorrentDownloadedState>) -> HttpAnnounceContext {
    HttpAnnounceContext {
        client: test_http_client(),
        torrent_meta: test_meta(url.clone()),
        peer_id: *b"-BR0100-0123456789ab",
        tracker_url: url,
        announce_key: 0xDF45_C574,
        port: 6881,
        download_state: Arc::new(Mutex::new(DownloadState::Downloading)),
        torrent_downloaded_state: download,
    }
}

fn test_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(identity::user_agent())
        .redirect(reqwest::redirect::Policy::limited(tracker::REDIRECT_LIMIT))
        .gzip(true)
        .http1_only()
        .build()
        .expect("test tracker HTTP client")
}

async fn settle_io() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(30)))
        .await
        .unwrap();
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn announce_sends_bitrev_user_agent() {
    let (url, mut rx, handle) = spawn_scripted_tracker(vec![MockResponse {
        status: 200,
        body: ok_announce(1800, None, None),
    }])
    .await;

    announce(&http_client(), &url).await.unwrap();
    let request = rx.recv().await.expect("announce request");
    assert_eq!(
        request.headers.get("user-agent"),
        Some(&identity::user_agent())
    );
    assert_eq!(
        request.headers.get("user-agent").map(String::as_str),
        Some("bitrev/0.1.0")
    );
    handle.abort();
}

#[tokio::test]
async fn announce_non_2xx_is_typed_error() {
    let (url, _rx, handle) = spawn_scripted_tracker(vec![MockResponse {
        status: 500,
        body: b"nope".to_vec(),
    }])
    .await;

    let err = announce(&build_http_client(), &url).await.unwrap_err();
    assert!(matches!(err, TrackerError::HttpStatus { status: 500 }));
    handle.abort();
}

#[tokio::test]
async fn announce_failure_reason_is_typed_error() {
    let (url, _rx, handle) = spawn_scripted_tracker(vec![MockResponse {
        status: 200,
        body: failure_announce("unregistered torrent"),
    }])
    .await;

    let err = announce(&build_http_client(), &url).await.unwrap_err();
    match err {
        TrackerError::FailureReason(reason) => {
            assert_eq!(reason, "unregistered torrent");
        }
        other => panic!("expected FailureReason, got {other}"),
    }
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn announce_loop_respects_interval_and_echoes_tracker_id() {
    let (url, mut rx, handle) = spawn_scripted_tracker(vec![MockResponse {
        status: 200,
        body: ok_announce(1800, Some(1800), Some(b"tid")),
    }])
    .await;

    let peer_states = Arc::new(PeerStates::default());
    let shutdown = CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let ctx = announce_ctx(url, incomplete_download());
    let peers_for_loop = peer_states.clone();

    let loop_task = tokio::spawn(async move {
        run_http_announce_loop(ctx, loop_shutdown, |peers| {
            let peer_states = peers_for_loop.clone();
            async move {
                for peer in peers {
                    peer_states.add_if_not_seen(peer);
                }
            }
        })
        .await;
    });

    let first = rx.recv().await.expect("started announce");
    assert_eq!(first.path, "/announce");
    assert_eq!(first.query_param("event"), Some("started"));
    assert_eq!(
        first.headers.get("user-agent"),
        Some(&identity::user_agent())
    );
    assert!(first
        .headers
        .get("accept-encoding")
        .is_some_and(|v| v.contains("gzip")));
    settle_io().await;
    assert_eq!(peer_states.states.len(), 1);

    tokio::time::advance(Duration::from_secs(1799)).await;
    settle_io().await;
    assert!(rx.try_recv().is_err(), "re-announce sent before interval");

    tokio::time::advance(Duration::from_secs(2)).await;
    let second = rx.recv().await.expect("interval re-announce");
    assert_eq!(second.query_param("event"), None);
    assert_eq!(second.query_param("trackerid"), Some("tid"));

    shutdown.cancel();
    settle_io().await;
    let _ = loop_task.await;
    handle.abort();
}

#[tokio::test(start_paused = true)]
async fn announce_loop_retries_failure_reason_with_backoff() {
    let (url, mut rx, handle) = spawn_scripted_tracker(vec![
        MockResponse {
            status: 200,
            body: failure_announce("try again later"),
        },
        MockResponse {
            status: 200,
            body: ok_announce(1800, None, None),
        },
    ])
    .await;

    let shutdown = CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let ctx = announce_ctx(url, incomplete_download());
    let loop_task = tokio::spawn(async move {
        run_http_announce_loop(ctx, loop_shutdown, |_| async {}).await;
    });

    let first = rx.recv().await.expect("failed started announce");
    assert_eq!(first.query_param("event"), Some("started"));
    settle_io().await;

    tokio::time::advance(Duration::from_secs(14)).await;
    settle_io().await;
    assert!(rx.try_recv().is_err(), "retried before backoff elapsed");

    tokio::time::advance(Duration::from_secs(2)).await;
    let second = rx.recv().await.expect("backoff retry");
    assert_eq!(second.query_param("event"), Some("started"));

    shutdown.cancel();
    settle_io().await;
    let _ = loop_task.await;
    handle.abort();
}

#[tokio::test]
async fn announce_loop_sends_stopped_on_shutdown() {
    let (url, mut rx, handle) = spawn_scripted_tracker(vec![MockResponse {
        status: 200,
        body: ok_announce(1800, None, None),
    }])
    .await;

    let shutdown = CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let ctx = announce_ctx(url, incomplete_download());
    let loop_task = tokio::spawn(async move {
        run_http_announce_loop(ctx, loop_shutdown, |_| async {}).await;
    });

    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("started announce timed out")
        .expect("started announce");
    assert_eq!(first.query_param("event"), Some("started"));
    settle_io().await;

    shutdown.cancel();

    let stopped = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("stopped announce timed out")
        .expect("stopped announce");
    assert_eq!(stopped.query_param("event"), Some("stopped"));

    let _ = tokio::time::timeout(Duration::from_secs(2), loop_task).await;
    handle.abort();
}
