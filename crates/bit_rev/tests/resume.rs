use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_bytes::ByteBuf;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use bit_rev::bitfield::Bitfield;
use bit_rev::file::{Info, TorrentFile, TorrentMeta};
use bit_rev::handshake::Handshake;
use bit_rev::message::{self, BlockRequest, Message};
use bit_rev::protocol::Protocol;
use bit_rev::resume::{self, ResumeData, ResumeStatus};
use bit_rev::session::{AddTorrentOptions, Session, SessionOptions};
use bit_rev::torrent::Torrent;
use bit_rev::utils;

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(data);
    hasher.digest().bytes()
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bitrev-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn generated_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn torrent_meta(name: &str, data: &[u8], piece_length: i64) -> TorrentMeta {
    let mut pieces = Vec::new();
    for chunk in data.chunks(piece_length as usize) {
        pieces.extend_from_slice(&sha1(chunk));
    }
    TorrentMeta::new(TorrentFile {
        info: Info {
            name: name.into(),
            pieces: ByteBuf::from(pieces),
            piece_length,
            md5sum: None,
            length: Some(data.len() as i64),
            files: None,
            private: None,
            path: None,
            root_hash: None,
        },
        announce: None,
        nodes: None,
        encoding: None,
        httpseeds: None,
        announce_list: None,
        creation_date: None,
        comment: None,
        created_by: None,
    })
    .expect("torrent meta")
}

fn test_session(state_dir: Option<PathBuf>) -> Session {
    Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir,
        ..SessionOptions::default()
    })
}

fn bitfield_with(piece_count: usize, have: &[usize]) -> Bitfield {
    let mut bitfield = Bitfield::with_piece_count(piece_count);
    for &i in have {
        bitfield.set_piece(i);
    }
    bitfield
}

async fn write_msg(stream: &mut TcpStream, msg: Message) {
    stream
        .write_all(&message::serialize(Some(msg)))
        .await
        .unwrap();
}

struct RecordingSeeder {
    addr: SocketAddr,
    requested: Arc<Mutex<Vec<u32>>>,
    cancel: CancellationToken,
}

impl Drop for RecordingSeeder {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn start_recording_seeder(
    data: Vec<u8>,
    piece_length: i64,
    info_hash: [u8; 20],
    allowed_pieces: Option<HashSet<u32>>,
) -> RecordingSeeder {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requested = Arc::new(Mutex::new(Vec::new()));
    let cancel = CancellationToken::new();
    let requested_task = requested.clone();
    let cancel_task = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_task.cancelled() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let data = data.clone();
                    let requested = requested_task.clone();
                    let allowed = allowed_pieces.clone();
                    let cancel = cancel_task.clone();
                    tokio::spawn(async move {
                        let _ = serve_recording_peer(
                            stream,
                            info_hash,
                            &data,
                            piece_length,
                            requested,
                            allowed,
                            cancel,
                        )
                        .await;
                    });
                }
            }
        }
    });
    RecordingSeeder {
        addr,
        requested,
        cancel,
    }
}

async fn serve_recording_peer(
    mut stream: TcpStream,
    info_hash: [u8; 20],
    data: &[u8],
    piece_length: i64,
    requested: Arc<Mutex<Vec<u32>>>,
    allowed_pieces: Option<HashSet<u32>>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let _hs = Protocol::read_handshake(&mut stream).await?;
    let reply = Handshake::new(info_hash, *b"-RV0001-0123456789ab");
    Protocol::write_handshake(&mut stream, &reply).await?;

    let piece_count = data.len().div_ceil(piece_length as usize);
    let mut bf = Bitfield::with_piece_count(piece_count);
    for i in 0..piece_count {
        bf.set_piece(i);
    }
    write_msg(&mut stream, Message::Bitfield(bf.as_bytes().to_vec())).await;
    write_msg(&mut stream, Message::Unchoke).await;

    let proto = Protocol::connect(
        stream.peer_addr().unwrap(),
        info_hash,
        *b"-RV0001-0123456789ab",
    )
    .await?;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            msg = proto.read(&mut stream) => {
                let msg = match msg {
                    Ok(Some(msg)) => msg,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                match msg {
                    Message::Interested => {
                        write_msg(&mut stream, Message::Unchoke).await;
                    }
                    Message::Request(payload) => {
                        let Some(req) = BlockRequest::from_payload(&payload) else {
                            continue;
                        };
                        requested.lock().unwrap().push(req.index);
                        if let Some(allowed) = &allowed_pieces {
                            if !allowed.contains(&req.index) {
                                continue;
                            }
                        }
                        let start = req.index as usize * piece_length as usize + req.begin as usize;
                        let end = (start + req.length as usize).min(data.len());
                        if start >= data.len() || start >= end {
                            continue;
                        }
                        write_msg(
                            &mut stream,
                            message::format_piece(req.index, req.begin, data[start..end].to_vec()),
                        )
                        .await;
                    }
                    Message::NotInterested | Message::Bitfield(_) | Message::Have(_) | Message::KeepAlive | Message::Choke | Message::Unchoke => {}
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn assert_file_hash(path: &std::path::Path, expected: &[u8]) {
    let got = std::fs::read(path).expect("read output");
    assert_eq!(got.len(), expected.len());
    assert_eq!(got, expected);
}

#[tokio::test]
async fn kill_and_restart_continues_from_persisted_bitfield() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(PIECE_LEN as usize * 4 + 100);
    let meta = torrent_meta("resume.bin", &data, PIECE_LEN);
    let piece_count = meta.piece_hashes.len();
    assert!(piece_count > 2);

    let state_dir = unique_temp_dir("resume-kill");
    let output = state_dir.join("out.bin");
    let first_seeder = start_recording_seeder(
        data.clone(),
        PIECE_LEN,
        meta.info_hash,
        Some(HashSet::from([0])),
    )
    .await;

    let first = test_session(Some(state_dir.clone()));
    let _ = tokio::time::timeout(Duration::from_secs(2), first.wait_listening()).await;
    let add = first
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output.clone()))
        .await
        .expect("add first leecher");
    assert!(first.connect_peer(&meta.info_hash, first_seeder.addr));

    let first_piece = tokio::time::timeout(Duration::from_secs(5), add.pr_rx.recv_async())
        .await
        .expect("first piece timeout")
        .expect("first piece");
    assert_eq!(first_piece.index, 0);

    first.flush_resume().await;
    drop(first);
    drop(first_seeder);

    let second = test_session(Some(state_dir.clone()));
    let _ = tokio::time::timeout(Duration::from_secs(2), second.wait_listening()).await;
    let add = second
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output.clone()))
        .await
        .expect("add second leecher");
    assert_eq!(add.resume_status, ResumeStatus::FastPath);
    assert!(
        add.already_have.iter().any(|pr| pr.index == 0),
        "restart should trust persisted piece 0"
    );
    let already: HashSet<u32> = add.already_have.iter().map(|pr| pr.index).collect();
    assert!(!already.contains(&1));

    let second_seeder = start_recording_seeder(data.clone(), PIECE_LEN, meta.info_hash, None).await;
    assert!(second.connect_peer(&meta.info_hash, second_seeder.addr));

    let mut have = already.clone();
    while have.len() < piece_count {
        let pr = tokio::time::timeout(Duration::from_secs(5), add.pr_rx.recv_async())
            .await
            .expect("resume piece timeout")
            .expect("resume piece");
        have.insert(pr.index);
    }

    second.flush_resume().await;
    let requested: HashSet<u32> = second_seeder
        .requested
        .lock()
        .unwrap()
        .iter()
        .copied()
        .collect();
    for index in &already {
        assert!(
            !requested.contains(index),
            "already-have piece {index} was requested after resume"
        );
    }
    assert!(
        requested.iter().any(|i| !already.contains(i)),
        "expected requests for missing pieces, got {requested:?}"
    );

    assert_file_hash(&output, &data);
    for (i, hash) in meta.piece_hashes.iter().enumerate() {
        let start = i * PIECE_LEN as usize;
        let end = (start + PIECE_LEN as usize).min(data.len());
        assert!(utils::check_integrity(hash, &data[start..end]));
    }
    drop(second);
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn crash_without_flush_does_not_corrupt_output() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(PIECE_LEN as usize * 3);
    let meta = torrent_meta("crash.bin", &data, PIECE_LEN);
    let state_dir = unique_temp_dir("resume-crash");
    let output = state_dir.join("out.bin");

    let seeder = start_recording_seeder(data.clone(), PIECE_LEN, meta.info_hash, None).await;
    let first = test_session(Some(state_dir.clone()));
    let _ = tokio::time::timeout(Duration::from_secs(2), first.wait_listening()).await;
    let add = first
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output.clone()))
        .await
        .unwrap();
    assert!(first.connect_peer(&meta.info_hash, seeder.addr));

    let mut have = HashSet::new();
    while have.len() < 2 {
        let pr = tokio::time::timeout(Duration::from_secs(5), add.pr_rx.recv_async())
            .await
            .expect("piece timeout")
            .unwrap();
        have.insert(pr.index);
    }
    drop(first);
    drop(seeder);

    let on_disk = std::fs::read(&output).unwrap();
    assert!(on_disk.len() >= PIECE_LEN as usize);

    let second = test_session(Some(state_dir.clone()));
    let add = second
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output.clone()))
        .await
        .unwrap();
    assert!(matches!(
        add.resume_status,
        ResumeStatus::FastPath | ResumeStatus::SlowPath
    ));
    let seeder = start_recording_seeder(data.clone(), PIECE_LEN, meta.info_hash, None).await;
    assert!(second.connect_peer(&meta.info_hash, seeder.addr));

    let mut have: HashSet<u32> = add.already_have.iter().map(|pr| pr.index).collect();
    while have.len() < meta.piece_hashes.len() {
        let pr = tokio::time::timeout(Duration::from_secs(5), add.pr_rx.recv_async())
            .await
            .expect("finish timeout")
            .unwrap();
        have.insert(pr.index);
    }
    assert_file_hash(&output, &data);
    drop(second);
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn fast_path_trusts_bitfield_when_mtimes_match() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(PIECE_LEN as usize * 2);
    let meta = torrent_meta("fast.bin", &data, PIECE_LEN);
    let torrent = Torrent::new(&meta).unwrap();
    let state_dir = unique_temp_dir("resume-fast");
    let output = state_dir.join("out.bin");
    std::fs::write(&output, &data).unwrap();

    let layout = resume::collect_file_layout(&torrent, &output);
    let bitfield = bitfield_with(meta.piece_hashes.len(), &[0]);
    let resume_data = ResumeData {
        version: resume::RESUME_VERSION,
        info_hash: ByteBuf::from(meta.info_hash.to_vec()),
        bitfield: ByteBuf::from(bitfield.as_bytes().to_vec()),
        output_dir: output.to_string_lossy().into_owned(),
        files: layout,
        uploaded: 99,
        downloaded: PIECE_LEN,
        torrent_path: resume::torrent_cache_path(&state_dir, &meta.info_hash)
            .to_string_lossy()
            .into_owned(),
        paused: 0,
        added_at: 1,
        completed_at: 0,
    };
    resume::save(
        &resume::resume_path(&state_dir, &meta.info_hash),
        &resume_data,
    )
    .unwrap();

    let session = test_session(Some(state_dir.clone()));
    let add = session
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output.clone()))
        .await
        .unwrap();
    assert_eq!(add.resume_status, ResumeStatus::FastPath);
    assert_eq!(add.already_have.len(), 1);
    assert_eq!(add.already_have[0].index, 0);
    assert_eq!(session.torrent_uploaded(&meta.info_hash), Some(99));
    drop(session);
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn slow_path_on_mtime_mismatch_rehashes() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(PIECE_LEN as usize * 2);
    let meta = torrent_meta("slow.bin", &data, PIECE_LEN);
    let torrent = Torrent::new(&meta).unwrap();
    let state_dir = unique_temp_dir("resume-slow");
    let output = state_dir.join("out.bin");
    std::fs::write(&output, &data).unwrap();

    let mut layout = resume::collect_file_layout(&torrent, &output);
    layout[0].mtime += 5;
    let bitfield = bitfield_with(meta.piece_hashes.len(), &[0]);
    let resume_data = ResumeData {
        version: resume::RESUME_VERSION,
        info_hash: ByteBuf::from(meta.info_hash.to_vec()),
        bitfield: ByteBuf::from(bitfield.as_bytes().to_vec()),
        output_dir: output.to_string_lossy().into_owned(),
        files: layout,
        uploaded: 0,
        downloaded: PIECE_LEN,
        torrent_path: String::new(),
        paused: 0,
        added_at: 1,
        completed_at: 0,
    };
    resume::save(
        &resume::resume_path(&state_dir, &meta.info_hash),
        &resume_data,
    )
    .unwrap();

    let session = test_session(Some(state_dir.clone()));
    let add = session
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output.clone()))
        .await
        .unwrap();
    assert_eq!(add.resume_status, ResumeStatus::SlowPath);
    assert_eq!(add.already_have.len(), meta.piece_hashes.len());
    drop(session);
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn verify_flag_forces_slow_path() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(PIECE_LEN as usize);
    let meta = torrent_meta("verify.bin", &data, PIECE_LEN);
    let torrent = Torrent::new(&meta).unwrap();
    let state_dir = unique_temp_dir("resume-verify");
    let output = state_dir.join("out.bin");
    std::fs::write(&output, &data).unwrap();

    let layout = resume::collect_file_layout(&torrent, &output);
    let bitfield = bitfield_with(1, &[0]);
    let resume_data = ResumeData {
        version: resume::RESUME_VERSION,
        info_hash: ByteBuf::from(meta.info_hash.to_vec()),
        bitfield: ByteBuf::from(bitfield.as_bytes().to_vec()),
        output_dir: output.to_string_lossy().into_owned(),
        files: layout,
        uploaded: 0,
        downloaded: PIECE_LEN,
        torrent_path: String::new(),
        paused: 0,
        added_at: 1,
        completed_at: 0,
    };
    resume::save(
        &resume::resume_path(&state_dir, &meta.info_hash),
        &resume_data,
    )
    .unwrap();

    let session = test_session(Some(state_dir.clone()));
    let add = session
        .add_torrent(
            AddTorrentOptions::from(meta.clone())
                .output_dir(output)
                .verify(true),
        )
        .await
        .unwrap();
    assert_eq!(add.resume_status, ResumeStatus::SlowPath);
    drop(session);
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn corrupt_resume_file_starts_fresh() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(PIECE_LEN as usize);
    let meta = torrent_meta("corrupt.bin", &data, PIECE_LEN);
    let state_dir = unique_temp_dir("resume-corrupt-add");
    let output = state_dir.join("out.bin");
    std::fs::write(&output, &data).unwrap();
    let path = resume::resume_path(&state_dir, &meta.info_hash);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{{{{not-bencode").unwrap();

    let session = test_session(Some(state_dir.clone()));
    let add = session
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output))
        .await
        .unwrap();
    assert_eq!(add.resume_status, ResumeStatus::Corrupt);
    assert!(add.already_have.is_empty());
    drop(session);
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn paused_flag_survives_restart() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(PIECE_LEN as usize);
    let meta = torrent_meta("paused.bin", &data, PIECE_LEN);
    let state_dir = unique_temp_dir("resume-paused");
    let output = state_dir.join("out.bin");

    let first = test_session(Some(state_dir.clone()));
    first
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output.clone()))
        .await
        .unwrap();
    first.pause();
    first.flush_resume().await;
    assert!(first.is_paused());
    drop(first);

    let second = test_session(Some(state_dir.clone()));
    second
        .add_torrent(AddTorrentOptions::from(meta).output_dir(output))
        .await
        .unwrap();
    assert!(second.is_paused());
    drop(second);
    let _ = std::fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn torrent_metainfo_is_cached_on_add() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(32);
    let meta = torrent_meta("cache.bin", &data, PIECE_LEN);
    let state_dir = unique_temp_dir("resume-cache");
    let output = state_dir.join("out.bin");
    let session = test_session(Some(state_dir.clone()));
    session
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(output))
        .await
        .unwrap();
    let cached = resume::torrent_cache_path(&state_dir, &meta.info_hash);
    assert!(cached.exists());
    let loaded = bit_rev::file::from_filename(cached.to_str().unwrap()).unwrap();
    assert_eq!(loaded.info_hash, meta.info_hash);
    drop(session);
    let _ = std::fs::remove_dir_all(&state_dir);
}
