#![allow(dead_code)]

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_bencode::ser;
use serde_bytes::ByteBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::compact_peers;

#[derive(Clone, Debug)]
pub struct RecordedHttpRequest {
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub peer_addr: SocketAddr,
}

impl RecordedHttpRequest {
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then_some(v)
        })
    }
}

#[derive(Clone, Debug)]
pub enum HttpAnnounceBody {
    Peers {
        interval: i64,
        min_interval: Option<i64>,
        peers: Vec<SocketAddr>,
        warning: Option<String>,
        tracker_id: Option<Vec<u8>>,
    },
    Failure(String),
    Raw {
        status: u16,
        body: Vec<u8>,
    },
    /// Accept the request and never write a response.
    Hang,
}

#[derive(Serialize)]
struct BencodeAnnounce {
    interval: i64,
    #[serde(rename = "min interval", skip_serializing_if = "Option::is_none")]
    min_interval: Option<i64>,
    #[serde(rename = "tracker id", skip_serializing_if = "Option::is_none")]
    tracker_id: Option<ByteBuf>,
    #[serde(rename = "warning message", skip_serializing_if = "Option::is_none")]
    warning_message: Option<String>,
    peers: ByteBuf,
}

impl HttpAnnounceBody {
    pub fn peers(interval: i64, peers: Vec<SocketAddr>) -> Self {
        Self::Peers {
            interval,
            min_interval: None,
            peers,
            warning: None,
            tracker_id: None,
        }
    }

    fn into_http(self) -> Option<(u16, Vec<u8>)> {
        match self {
            Self::Hang => None,
            Self::Raw { status, body } => Some((status, body)),
            Self::Failure(reason) => Some((
                200,
                format!("d14:failure reason{}:{reason}e", reason.len()).into_bytes(),
            )),
            Self::Peers {
                interval,
                min_interval,
                peers,
                warning,
                tracker_id,
            } => {
                let body = ser::to_bytes(&BencodeAnnounce {
                    interval,
                    min_interval,
                    tracker_id: tracker_id.map(ByteBuf::from),
                    warning_message: warning,
                    peers: ByteBuf::from(compact_peers(&peers)),
                })
                .expect("bencode announce");
                Some((200, body))
            }
        }
    }
}

pub struct MockHttpTracker {
    pub addr: SocketAddr,
    pub url: String,
    requests: Arc<Mutex<Vec<RecordedHttpRequest>>>,
    request_notify: Arc<Notify>,
    cancel: CancellationToken,
}

impl Drop for MockHttpTracker {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl MockHttpTracker {
    pub async fn start(responses: Vec<HttpAnnounceBody>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock http tracker");
        let addr = listener.local_addr().expect("local addr");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let request_notify = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let idx = Arc::new(AtomicUsize::new(0));
        let responses = Arc::new(responses);
        let requests_task = requests.clone();
        let notify_task = request_notify.clone();
        let cancel_task = cancel.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_task.cancelled() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, peer_addr)) = accepted else { break };
                        let responses = responses.clone();
                        let idx = idx.clone();
                        let requests = requests_task.clone();
                        let request_notify = notify_task.clone();
                        let cancel = cancel_task.clone();
                        tokio::spawn(async move {
                            let _ = handle_conn(
                                stream,
                                peer_addr,
                                responses,
                                idx,
                                requests,
                                request_notify,
                                cancel,
                            )
                            .await;
                        });
                    }
                }
            }
        });

        Self {
            addr,
            url: format!("http://{addr}/announce"),
            requests,
            request_notify,
            cancel,
        }
    }

    pub fn requests(&self) -> Vec<RecordedHttpRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub async fn wait_requests(&self, n: usize, timeout: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.requests().len() >= n {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!(
                    "http tracker saw {} requests, wanted {n} before timeout",
                    self.requests().len()
                );
            }
            tokio::select! {
                _ = self.request_notify.notified() => {}
                _ = tokio::time::sleep(remaining) => {
                    panic!(
                        "http tracker saw {} requests, wanted {n} before timeout",
                        self.requests().len()
                    );
                }
            }
        }
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    responses: Arc<Vec<HttpAnnounceBody>>,
    idx: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<RecordedHttpRequest>>>,
    request_notify: Arc<Notify>,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    let request = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = read_http_request(&mut stream) => result?,
    };
    let mut recorded = request;
    recorded.peer_addr = peer_addr;
    requests.lock().unwrap().push(recorded);
    request_notify.notify_waiters();

    let i = idx.fetch_add(1, Ordering::SeqCst);
    let body = responses
        .get(i)
        .cloned()
        .or_else(|| responses.last().cloned())
        .unwrap_or_else(|| HttpAnnounceBody::peers(1800, Vec::new()));

    let Some((status, body)) = body.into_http() else {
        cancel.cancelled().await;
        return Ok(());
    };

    let header = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: text/plain\r\n\r\n",
        status_reason(status),
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&body).await?;
    stream.shutdown().await
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        403 => "Forbidden",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

async fn read_http_request(stream: &mut TcpStream) -> std::io::Result<RecordedHttpRequest> {
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

    Ok(RecordedHttpRequest {
        path: path.to_string(),
        query: query.to_string(),
        headers,
        peer_addr: "127.0.0.1:0".parse().unwrap(),
    })
}
