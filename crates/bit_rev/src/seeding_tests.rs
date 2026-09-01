use std::path::PathBuf;
use std::time::Duration;

use serde_bytes::ByteBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::file::{Info, TorrentFile, TorrentMeta};
use crate::handshake::Handshake;
use crate::message::{self, BlockRequest, Message, MAX_INCOMING_REQUEST_LENGTH};
use crate::protocol::Protocol;
use crate::session::{AddTorrentOptions, Session, SessionOptions};
use crate::utils;

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

async fn start_seeder(
    data: &[u8],
    piece_length: i64,
) -> (Session, std::net::SocketAddr, TorrentMeta) {
    let dir = unique_temp_dir("seed");
    let file_path = dir.join("tiny.bin");
    std::fs::write(&file_path, data).unwrap();
    let meta = torrent_meta("tiny.bin", data, piece_length);

    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir: None,
        ..SessionOptions::default()
    });
    let addr = tokio::time::timeout(Duration::from_secs(2), session.wait_listening())
        .await
        .expect("listener bound");

    session
        .add_torrent(
            AddTorrentOptions::from(meta.clone())
                .output_dir(file_path)
                .seed(true),
        )
        .await
        .expect("add seeder torrent");

    (session, addr, meta)
}

async fn handshake_with(
    addr: std::net::SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
) -> (TcpStream, Protocol) {
    let mut stream = TcpStream::connect(addr).await.expect("connect seeder");
    let local = Handshake::new(info_hash, peer_id);
    stream.write_all(&local.serialize()).await.unwrap();
    let proto = Protocol::connect(addr, info_hash, peer_id).await.unwrap();
    let reply = Protocol::read_handshake(&mut stream).await.unwrap();
    assert_eq!(reply.info_hash, info_hash);
    (stream, proto)
}

async fn write_msg(stream: &mut TcpStream, msg: Message) {
    stream
        .write_all(&message::serialize(Some(msg)))
        .await
        .unwrap();
}

async fn drain_until_bitfield(proto: &Protocol, stream: &mut TcpStream) {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(2), proto.read(&mut *stream))
            .await
            .expect("bitfield timeout")
            .expect("bitfield read");
        match msg {
            Some(Message::Bitfield(_)) => return,
            Some(Message::KeepAlive) | None => continue,
            Some(other) => panic!("expected bitfield first, got {other:?}"),
        }
    }
}

async fn expect_disconnect(proto: &Protocol, stream: &mut TcpStream) {
    let result = tokio::time::timeout(Duration::from_secs(2), proto.read(&mut *stream)).await;
    match result {
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {}
        Ok(Ok(Some(msg))) => panic!("expected disconnect, got {msg:?}"),
    }
}

#[tokio::test]
async fn seeder_serves_full_torrent_to_in_process_leecher() {
    const PIECE_LEN: i64 = 16_384;
    let data = generated_payload(40_000);
    let (session, addr, meta) = start_seeder(&data, PIECE_LEN).await;
    let info_hash = meta.info_hash;
    let peer_id = *b"-LC0001-0123456789ab";

    let (mut stream, proto) = handshake_with(addr, info_hash, peer_id).await;

    let mut have = vec![false; meta.piece_hashes.len()];
    let mut unchoked = false;
    let mut requested = false;
    let mut assembled = vec![0u8; data.len()];
    let mut received = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    while received < data.len() {
        if tokio::time::Instant::now() > deadline {
            panic!("leecher timed out after {received} bytes");
        }
        let msg = tokio::time::timeout(Duration::from_secs(2), proto.read(&mut stream))
            .await
            .expect("read timeout")
            .expect("read error");
        let Some(msg) = msg else {
            continue;
        };
        match msg {
            Message::Bitfield(bytes) => {
                let bf = crate::bitfield::Bitfield::new(bytes);
                for (i, slot) in have.iter_mut().enumerate() {
                    *slot = bf.has_piece(i);
                }
            }
            Message::Have(index) => {
                if let Some(slot) = have.get_mut(index as usize) {
                    *slot = true;
                }
            }
            Message::Unchoke => {
                unchoked = true;
            }
            Message::Piece(chunk) => {
                let start = chunk.index as usize * PIECE_LEN as usize + chunk.start as usize;
                assembled[start..start + chunk.data.len()].copy_from_slice(&chunk.data);
                received += chunk.data.len();
            }
            Message::Choke => {
                unchoked = false;
            }
            _ => {}
        }

        if have.iter().any(|h| *h) && !requested {
            write_msg(&mut stream, Message::Interested).await;
        }
        if unchoked && !requested {
            let mut offset = 0u32;
            let total = data.len() as u32;
            let mut index = 0u32;
            while offset < total {
                let piece_start = index * PIECE_LEN as u32;
                let piece_len = utils::calculate_piece_size(
                    &crate::torrent::Torrent::new(&meta).unwrap(),
                    index as usize,
                ) as u32;
                let mut begin = 0u32;
                while begin < piece_len {
                    let length = utils::calculate_block_size(piece_len, begin);
                    write_msg(&mut stream, message::format_request(index, begin, length)).await;
                    begin += length;
                    offset = piece_start + begin;
                }
                index += 1;
            }
            requested = true;
        }
    }

    assert_eq!(assembled, data);
    for (i, hash) in meta.piece_hashes.iter().enumerate() {
        let start = i * PIECE_LEN as usize;
        let end = (start + PIECE_LEN as usize).min(data.len());
        assert!(utils::check_integrity(hash, &assembled[start..end]));
    }
    assert!(
        session.uploaded() >= data.len() as u64,
        "uploaded counter should grow, got {}",
        session.uploaded()
    );
    assert_eq!(
        session.torrent_uploaded(&info_hash),
        Some(session.uploaded())
    );
}

#[tokio::test]
async fn seeder_disconnects_on_oversized_request() {
    let data = generated_payload(16_384);
    let (session, addr, meta) = start_seeder(&data, 16_384).await;
    let (mut stream, proto) = handshake_with(addr, meta.info_hash, *b"-LC0001-oversize0001").await;
    drain_until_bitfield(&proto, &mut stream).await;

    write_msg(
        &mut stream,
        message::format_request(0, 0, MAX_INCOMING_REQUEST_LENGTH + 1),
    )
    .await;

    expect_disconnect(&proto, &mut stream).await;
    drop(session);
}

#[tokio::test]
async fn seeder_disconnects_on_out_of_bounds_request() {
    let data = generated_payload(16_384);
    let (session, addr, meta) = start_seeder(&data, 16_384).await;
    let (mut stream, proto) = handshake_with(addr, meta.info_hash, *b"-LC0001-outofbound01").await;
    drain_until_bitfield(&proto, &mut stream).await;

    write_msg(
        &mut stream,
        Message::Request(
            BlockRequest {
                index: 0,
                begin: 16_384,
                length: 1,
            }
            .to_payload(),
        ),
    )
    .await;

    expect_disconnect(&proto, &mut stream).await;
    drop(session);
}

#[tokio::test]
async fn incoming_unknown_info_hash_is_closed() {
    let data = generated_payload(16_384);
    let (session, addr, _meta) = start_seeder(&data, 16_384).await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let hs = Handshake::new([0xABu8; 20], *b"-LC0001-unknown00001");
    stream.write_all(&hs.serialize()).await.unwrap();

    let mut buf = [0u8; 1];
    let result = tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut buf)).await;
    assert!(
        matches!(result, Ok(Err(_)) | Err(_) | Ok(Ok(0))),
        "unknown hash should close, got {result:?}"
    );
    drop(session);
}
