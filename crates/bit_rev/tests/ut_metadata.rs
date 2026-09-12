mod common;

use std::net::SocketAddr;
use std::time::Duration;

use bit_rev::extension::handshake::{ExtensionHandshake, UT_METADATA};
use bit_rev::extension::ut_metadata::{
    encode_data, parse_message, UtMetadataMessage, METADATA_PIECE_LEN,
};
use bit_rev::extension::MAX_METADATA_SIZE;
use bit_rev::handshake::Handshake;
use bit_rev::message::{format_extended, Message};
use bit_rev::protocol::{Frame, Protocol};
use bit_rev::session::{AddInfoHashOptions, AddTorrentOptions};
use bit_rev::torrent::Torrent;
use common::{test_session, unique_temp_dir, TorrentFixture};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn fetches_verified_metadata_from_seeder_session() {
    let fixture = TorrentFixture::single(64 * 1024, 16 * 1024, 99);
    let expected = fixture.torrent();

    let seed_dir = unique_temp_dir();
    let seed_path = fixture.session_output(seed_dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &seed_path).expect("copy seed payload");

    let seeder = test_session(None).await;
    seeder
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(seed_path)
                .seed(true),
        )
        .await
        .expect("add seeder torrent");
    let seeder_addr = seeder.wait_listening().await;

    let leecher = test_session(None).await;
    let handle = leecher
        .add_torrent_by_info_hash(fixture.torrent_meta.info_hash, AddInfoHashOptions::new())
        .await
        .expect("add by info hash");
    assert!(leecher.connect_peer(&fixture.torrent_meta.info_hash, seeder_addr));

    let meta = tokio::time::timeout(Duration::from_secs(10), handle.wait_meta())
        .await
        .expect("metadata timeout")
        .expect("metadata parse");
    let got = Torrent::new(&meta).expect("torrent from fetched meta");
    assert_eq!(got.info_hash, expected.info_hash);
    assert_eq!(got.piece_hashes, expected.piece_hashes);
    assert_eq!(got.files, expected.files);
    assert_eq!(got.name, expected.name);
    assert_eq!(meta.info_hash, fixture.torrent_meta.info_hash);
    assert_eq!(
        meta.info_bytes.as_ref(),
        fixture.torrent_meta.info_bytes.as_ref()
    );
}

#[tokio::test]
async fn fetches_multi_piece_metadata() {
    // ~900 piece hashes make the info dict larger than 16 KiB.
    let fixture = TorrentFixture::single(900 * 32, 32, 7);
    assert!(
        fixture.torrent_meta.info_bytes.len() > METADATA_PIECE_LEN,
        "fixture info dict should span multiple metadata pieces"
    );
    let expected = fixture.torrent();

    let seed_dir = unique_temp_dir();
    let seed_path = fixture.session_output(seed_dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &seed_path).expect("copy seed payload");

    let seeder = test_session(None).await;
    seeder
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(seed_path)
                .seed(true),
        )
        .await
        .expect("add seeder torrent");
    let seeder_addr = seeder.wait_listening().await;

    let leecher = test_session(None).await;
    let handle = leecher
        .add_torrent_by_info_hash(fixture.torrent_meta.info_hash, AddInfoHashOptions::new())
        .await
        .expect("add by info hash");
    assert!(leecher.connect_peer(&fixture.torrent_meta.info_hash, seeder_addr));

    let meta = tokio::time::timeout(Duration::from_secs(15), handle.wait_meta())
        .await
        .expect("metadata timeout")
        .expect("metadata parse");
    let got = Torrent::new(&meta).expect("torrent from fetched meta");
    assert_eq!(got.info_hash, expected.info_hash);
    assert_eq!(got.piece_hashes, expected.piece_hashes);
    assert_eq!(got.files, expected.files);
}

#[tokio::test]
async fn corrupt_metadata_is_penalized_then_honest_peer_succeeds() {
    let fixture = TorrentFixture::single(32 * 1024, 16 * 1024, 3);
    let expected = fixture.torrent();
    let mut bad = fixture.torrent_meta.info_bytes.to_vec();
    bad[0] ^= 0xff;

    let corrupt = start_metadata_peer(
        fixture.torrent_meta.info_hash,
        fixture.torrent_meta.info_bytes.len() as i64,
        bad,
    )
    .await;

    let leecher = test_session(None).await;
    let handle = leecher
        .add_torrent_by_info_hash(fixture.torrent_meta.info_hash, AddInfoHashOptions::new())
        .await
        .expect("add by info hash");
    assert!(leecher.connect_peer(&fixture.torrent_meta.info_hash, corrupt.addr));

    let banned = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if handle.peer_states().is_banned(corrupt.addr) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(banned.is_ok(), "corrupt metadata peer should be banned");
    assert!(handle.store().info_bytes().is_none());

    let seed_dir = unique_temp_dir();
    let seed_path = fixture.session_output(seed_dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &seed_path).expect("copy seed payload");
    let seeder = test_session(None).await;
    seeder
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(seed_path)
                .seed(true),
        )
        .await
        .expect("add seeder torrent");
    let seeder_addr = seeder.wait_listening().await;
    assert!(leecher.connect_peer(&fixture.torrent_meta.info_hash, seeder_addr));

    let meta = tokio::time::timeout(Duration::from_secs(10), handle.wait_meta())
        .await
        .expect("metadata timeout")
        .expect("metadata parse");
    let got = Torrent::new(&meta).expect("torrent");
    assert_eq!(got.info_hash, expected.info_hash);
    assert_eq!(got.piece_hashes, expected.piece_hashes);
    assert!(handle.peer_states().is_banned(corrupt.addr));
}

struct MockMetadataPeer {
    addr: SocketAddr,
    cancel: CancellationToken,
}

impl Drop for MockMetadataPeer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn start_metadata_peer(
    info_hash: [u8; 20],
    metadata_size: i64,
    payload: Vec<u8>,
) -> MockMetadataPeer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cancel = CancellationToken::new();
    let cancel_task = cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_task.cancelled() => break,
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else { break };
                    let payload = payload.clone();
                    let cancel = cancel_task.clone();
                    tokio::spawn(async move {
                        let _ = serve_metadata_peer(
                            stream,
                            peer,
                            info_hash,
                            metadata_size,
                            payload,
                            cancel,
                        )
                        .await;
                    });
                }
            }
        }
    });
    MockMetadataPeer { addr, cancel }
}

async fn serve_metadata_peer(
    mut stream: TcpStream,
    peer: SocketAddr,
    info_hash: [u8; 20],
    metadata_size: i64,
    payload: Vec<u8>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let protocol = Protocol::connect(peer, info_hash, *b"-RV0001-metadatapeer").await?;
    let incoming = Protocol::read_handshake(&mut stream).await?;
    let reply = Handshake::outgoing(info_hash, *b"-RV0001-metadatapeer");
    Protocol::write_handshake(&mut stream, &reply).await?;

    if !incoming.supports_extension_protocol() {
        return Ok(());
    }

    let mut m = std::collections::BTreeMap::new();
    m.insert(UT_METADATA.into(), 1);
    let hs = ExtensionHandshake::outgoing(m, None, Some(metadata_size));
    protocol
        .send_message(&mut stream, format_extended(0, hs.encode()))
        .await?;

    let mut peer_ut = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            frame = protocol.read(&mut stream) => {
                match frame? {
                    Frame::Eof => return Ok(()),
                    Frame::Message(Message::Extended { ext_id, payload: body }) => {
                        if ext_id == 0 {
                            let decoded = ExtensionHandshake::decode(&body);
                            peer_ut = decoded.m.get(UT_METADATA).and_then(|id| u8::try_from(*id).ok());
                            continue;
                        }
                        if ext_id != 1 {
                            continue;
                        }
                        let Some(UtMetadataMessage::Request { piece }) = parse_message(&body) else {
                            continue;
                        };
                        let start = piece as usize * METADATA_PIECE_LEN;
                        if start >= payload.len() {
                            continue;
                        }
                        let end = (start + METADATA_PIECE_LEN).min(payload.len());
                        let Some(out_id) = peer_ut else { continue };
                        let msg = format_extended(
                            out_id,
                            encode_data(piece, metadata_size, &payload[start..end]),
                        );
                        protocol.send_message(&mut stream, msg).await?;
                    }
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn handshake_rejects_zero_and_huge_metadata_size() {
    assert!(ExtensionHandshake::decode(b"d13:metadata_sizei0ee")
        .metadata_size
        .is_none());
    let huge = format!("d13:metadata_sizei{}ee", MAX_METADATA_SIZE + 1);
    assert!(ExtensionHandshake::decode(huge.as_bytes())
        .metadata_size
        .is_none());
    assert_eq!(
        ExtensionHandshake::decode(b"d13:metadata_sizei8192ee").metadata_size,
        Some(8192)
    );
}
