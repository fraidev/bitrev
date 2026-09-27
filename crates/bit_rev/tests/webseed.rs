mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bit_rev::discovery::DiscoverySource;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use common::{
    add_download, test_session, unique_temp_dir, wait_for_completion, FileSpec, SeederConfig,
    SeederPeer, TorrentFixture, DOWNLOAD_TIMEOUT,
};

struct HttpSeed {
    base: String,
    hits: Arc<AtomicUsize>,
}

impl HttpSeed {
    async fn start(files: HashMap<String, Vec<u8>>, corrupt: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind web seed");
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let files = files.clone();
                let hits = hits_task.clone();
                tokio::spawn(async move {
                    let _ = serve(socket, &files, corrupt, &hits).await;
                });
            }
        });
        Self {
            base: format!("http://{addr}"),
            hits,
        }
    }

    fn requests(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    async fn wait_requests(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.requests() < n {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("web seed request count");
    }
}

async fn serve(
    mut socket: tokio::net::TcpStream,
    files: &HashMap<String, Vec<u8>>,
    corrupt: bool,
    hits: &AtomicUsize,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = socket.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines = text.lines();
    let request = lines.next().unwrap_or("");
    let mut parts = request.split_whitespace();
    let _method = parts.next();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = percent_decode(path);

    let mut range = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("range:") {
            range = parse_range(value.trim());
        }
    }

    hits.fetch_add(1, Ordering::Relaxed);

    let body = if let Some(piece) = query_param(query, "piece") {
        let piece: usize = piece.parse().unwrap_or(0);
        let payload = files.get("__payload__").map(Vec::as_slice).unwrap_or(&[]);
        let piece_len = files
            .get("__piece_len__")
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(payload.len().max(1));
        let start = piece * piece_len;
        let end = (start + piece_len).min(payload.len());
        payload.get(start..end).unwrap_or_default().to_vec()
    } else {
        let file = files.get(&path).cloned().unwrap_or_default();
        match range {
            Some((start, end)) if start < file.len() => {
                let end = end.min(file.len() - 1);
                file[start..=end].to_vec()
            }
            _ => file,
        }
    };
    let mut body = body;
    if corrupt && !body.is_empty() {
        body[0] ^= 0xff;
    }
    let status = if range.is_some() && query_param(query, "piece").is_none() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket.write_all(header.as_bytes()).await?;
    socket.write_all(&body).await?;
    Ok(())
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn parse_range(value: &str) -> Option<(usize, usize)> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn file_map(fixture: &TorrentFixture, prefix: &str) -> HashMap<String, Vec<u8>> {
    let mut files = HashMap::new();
    for file in &fixture.files {
        let mut path = prefix.to_string();
        for part in &file.path {
            path.push('/');
            path.push_str(part);
        }
        let bytes = std::fs::read(&file.disk_path).expect("fixture file");
        files.insert(path, bytes);
    }
    if let Some(payload) = fixture.payload_bytes() {
        files.insert("__payload__".into(), payload.to_vec());
    }
    files.insert(
        "__piece_len__".into(),
        fixture.piece_length.to_string().into_bytes(),
    );
    files
}

#[tokio::test]
async fn single_file_webseed_downloads_with_zero_peers() {
    let fixture = TorrentFixture::builder()
        .single_file("payload.bin", 48 * 1024)
        .piece_length(16 * 1024)
        .private(true)
        .seed(0x1951_0009)
        .build();
    let server = HttpSeed::start(file_map(&fixture, ""), false).await;
    let mut meta = fixture.torrent_meta.clone();
    meta.torrent_file.url_list = Some(vec![format!("{}/payload.bin", server.base)]);

    let session = test_session(None).await;
    let dir = unique_temp_dir();
    let output = fixture.session_output(dir.path());
    let added = add_download(&session, meta, output.clone()).await;
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    let snap = session.snapshot(added.id).expect("snapshot");
    assert_eq!(snap.peers, 0);
    assert_eq!(snap.downloaded, fixture.total_length);
    session.shutdown();
    fixture.assert_output_matches(&output);
    assert!(server.requests() >= fixture.piece_count());
}

#[tokio::test]
async fn multi_file_webseed_downloads_with_zero_peers() {
    let fixture = TorrentFixture::builder()
        .name("bundle")
        .piece_length(16 * 1024)
        .seed(0x194D_0017)
        .files(vec![
            FileSpec::new(["Readme.txt"], 1000),
            FileSpec::new(["dir", "a.bin"], 20_000),
        ])
        .build();
    let server = HttpSeed::start(file_map(&fixture, "/bundle"), false).await;
    let mut meta = fixture.torrent_meta.clone();
    meta.torrent_file.url_list = Some(vec![format!("{}/", server.base)]);

    let session = test_session(None).await;
    let dir = unique_temp_dir();
    let output = fixture.session_output(dir.path());
    let added = add_download(&session, meta, output.clone()).await;
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    let snap = session.snapshot(added.id).expect("snapshot");
    assert_eq!(snap.peers, 0);
    session.shutdown();
    fixture.assert_output_matches(&output);
}

#[tokio::test]
async fn corrupt_webseed_is_dropped_and_seeder_finishes() {
    let fixture = Arc::new(
        TorrentFixture::builder()
            .single_file("payload.bin", 48 * 1024)
            .piece_length(16 * 1024)
            .seed(0x19BA_D000)
            .build(),
    );
    let server = HttpSeed::start(file_map(&fixture, ""), true).await;
    let mut meta = fixture.torrent_meta.clone();
    meta.torrent_file.url_list = Some(vec![format!("{}/payload.bin", server.base)]);

    let session = test_session(None).await;
    let dir = unique_temp_dir();
    let output = fixture.session_output(dir.path());
    let added = add_download(&session, meta, output.clone()).await;
    server.wait_requests(3).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_drop = server.requests();

    let seeder = SeederPeer::start(fixture.clone(), SeederConfig::all_pieces()).await;
    session.add_peers(
        &fixture.torrent_meta.info_hash,
        DiscoverySource::Tracker,
        vec![seeder.addr],
    );
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    let snap = session.snapshot(added.id).expect("snapshot");
    assert!(snap.peers <= 1, "web seeds must not count as peers");
    assert!(
        server.requests() - after_drop <= 4,
        "url kept requesting after hash failures: {} then {}",
        after_drop,
        server.requests()
    );
    session.shutdown();
    fixture.assert_output_matches(&output);
}

#[tokio::test]
async fn httpseed_bep17_single_file() {
    let fixture = TorrentFixture::builder()
        .single_file("payload.bin", 32 * 1024)
        .piece_length(16 * 1024)
        .seed(0x175E_ED00)
        .build();
    let server = HttpSeed::start(file_map(&fixture, ""), false).await;
    let mut meta = fixture.torrent_meta.clone();
    meta.torrent_file.httpseeds = Some(vec![format!("{}/seed", server.base)]);

    let session = test_session(None).await;
    let dir = unique_temp_dir();
    let output = fixture.session_output(dir.path());
    let added = add_download(&session, meta, output.clone()).await;
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    assert_eq!(session.snapshot(added.id).unwrap().peers, 0);
    session.shutdown();
    fixture.assert_output_matches(&output);
}
