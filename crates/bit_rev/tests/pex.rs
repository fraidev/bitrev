mod common;

use std::net::SocketAddr;
use std::time::Duration;

use bit_rev::dht::DhtOptions;
use bit_rev::extension::handshake::{ExtensionHandshake, UT_PEX};
use bit_rev::extension::ut_pex::{PexMessage, PEX_FLAG_SEED};
use bit_rev::handshake::Handshake;
use bit_rev::message::{format_extended, Message};
use bit_rev::mse::EncryptionPolicy;
use bit_rev::peer::encode_compact_v4;
use bit_rev::protocol::{Frame, Protocol};
use bit_rev::session::{AddTorrentOptions, Session, SessionOptions};
use bit_rev::utp::UtpOptions;
use common::{
    test_session, unique_temp_dir, wait_for_completion, TorrentFixture, DOWNLOAD_TIMEOUT,
    LISTEN_TIMEOUT,
};
use tokio::net::TcpStream;

fn pex_options() -> SessionOptions {
    SessionOptions {
        listen_port: 0,
        state_dir: None,
        encryption: EncryptionPolicy::Disabled,
        dht: DhtOptions {
            enabled: false,
            port: 0,
            bootstrap_nodes: vec![],
        },
        utp: UtpOptions {
            enabled: false,
            port: 0,
        },
        ..SessionOptions::default()
    }
}

async fn pex_session() -> Session {
    let session = Session::with_options(pex_options());
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("session listen timeout");
    session
}

async fn add_seeder(
    session: &Session,
    fixture: &TorrentFixture,
) -> (tempfile::TempDir, SocketAddr) {
    let dir = unique_temp_dir();
    let path = fixture.session_output(dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &path).expect("copy seed payload");
    session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(path)
                .seed(true),
        )
        .await
        .expect("add seeder");
    let addr = session.wait_listening().await;
    (dir, addr)
}

async fn wait_contains_peer(session: &Session, info_hash: &[u8; 20], peer: SocketAddr) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let found = session
            .torrent_session(info_hash)
            .and_then(|t| {
                t.peer_states
                    .states
                    .get(&peer)
                    .map(|s| s.writer_tx.is_some())
            })
            .unwrap_or(false);
        if found {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for peer {peer}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pex_moves_compact_peer_from_a_to_b() {
    let fixture = TorrentFixture::single(64 * 1024, 16 * 1024, 0x9E11_0001);
    let info_hash = fixture.torrent_meta.info_hash;

    let c = pex_session().await;
    let _c_dir = add_seeder(&c, &fixture).await;
    let c_addr = c.wait_listening().await;

    let a = pex_session().await;
    let _a_dir = add_seeder(&a, &fixture).await;
    assert!(a.connect_peer(&info_hash, c_addr));
    wait_contains_peer(&a, &info_hash, c_addr).await;

    let b = pex_session().await;
    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let added = b
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone()).output_dir(output.clone()),
        )
        .await
        .expect("add leecher");
    let a_addr = a.wait_listening().await;
    assert!(b.connect_peer(&info_hash, a_addr));

    wait_contains_peer(&b, &info_hash, c_addr).await;
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;

    b.shutdown();
    a.shutdown();
    c.shutdown();
    fixture.assert_output_matches(&output);
}

#[tokio::test]
async fn private_torrent_does_not_advertise_or_honor_pex() {
    let fixture = TorrentFixture::builder()
        .single_file("payload.bin", 32 * 1024)
        .piece_length(16 * 1024)
        .seed(0x9E11_0002)
        .private(true)
        .build();
    assert!(!fixture.torrent().allows_pex());

    let seeder = pex_session().await;
    let _dir = add_seeder(&seeder, &fixture).await;
    let seeder_addr = seeder.wait_listening().await;
    let info_hash = fixture.torrent_meta.info_hash;

    let mut stream = TcpStream::connect(seeder_addr).await.expect("dial seeder");
    let protocol = Protocol::connect(seeder_addr, info_hash, *b"-RV0001-pexprivtest0")
        .await
        .expect("protocol");
    let local = Handshake::outgoing(info_hash, *b"-RV0001-pexprivtest0");
    Protocol::write_handshake(&mut stream, &local)
        .await
        .expect("write handshake");
    let incoming = Protocol::read_handshake(&mut stream)
        .await
        .expect("seeder handshake");
    assert!(incoming.supports_extension_protocol());

    let mut m = std::collections::BTreeMap::new();
    m.insert(UT_PEX.into(), 1);
    let hs = ExtensionHandshake::outgoing(m, Some(1), None);
    protocol
        .send_message(&mut stream, format_extended(0, hs.encode()))
        .await
        .expect("send ext handshake");

    let mut captured = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw_handshake = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, protocol.read(&mut stream)).await {
            Ok(Ok(Frame::Message(Message::Extended { ext_id, payload }))) => {
                captured.extend_from_slice(&payload);
                if ext_id == 0 {
                    let decoded = ExtensionHandshake::decode(&payload);
                    assert!(
                        !decoded.m.contains_key(UT_PEX),
                        "private torrent advertised ut_pex: {:?}",
                        decoded.m
                    );
                    saw_handshake = true;
                } else {
                    panic!("private torrent sent non-handshake extended id {ext_id}");
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
        if saw_handshake {
            break;
        }
    }
    assert!(saw_handshake, "did not receive extension handshake");
    assert!(
        !captured
            .windows(UT_PEX.len())
            .any(|w| w == UT_PEX.as_bytes()),
        "private torrent put ut_pex on the wire"
    );

    let crafted_peer: SocketAddr = "203.0.113.9:1234".parse().unwrap();
    let pex = PexMessage {
        added: vec![(crafted_peer, PEX_FLAG_SEED)],
        ..PexMessage::default()
    };
    // Unknown id: private torrents must not bind ut_pex.
    protocol
        .send_message(&mut stream, format_extended(2, pex.encode()))
        .await
        .expect("send crafted pex");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let states = seeder
        .torrent_session(&info_hash)
        .expect("seeder torrent")
        .peer_states
        .clone();
    assert!(
        !states.states.contains_key(&crafted_peer),
        "private torrent honored inbound PEX"
    );

    seeder.shutdown();
}

#[tokio::test]
async fn pex_disabled_does_not_advertise() {
    let fixture = TorrentFixture::single(16 * 1024, 16 * 1024, 0x9E11_0003);
    let mut options = pex_options();
    options.pex = false;
    let seeder = Session::with_options(options);
    tokio::time::timeout(LISTEN_TIMEOUT, seeder.wait_listening())
        .await
        .expect("listen");
    let _dir = add_seeder(&seeder, &fixture).await;
    let seeder_addr = seeder.wait_listening().await;
    let info_hash = fixture.torrent_meta.info_hash;

    let mut stream = TcpStream::connect(seeder_addr).await.expect("dial");
    let protocol = Protocol::connect(seeder_addr, info_hash, *b"-RV0001-pexdisabled0")
        .await
        .expect("protocol");
    let local = Handshake::outgoing(info_hash, *b"-RV0001-pexdisabled0");
    Protocol::write_handshake(&mut stream, &local)
        .await
        .expect("write hs");
    let _incoming = Protocol::read_handshake(&mut stream).await.expect("hs");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, protocol.read(&mut stream)).await {
            Ok(Ok(Frame::Message(Message::Extended { ext_id: 0, payload }))) => {
                let decoded = ExtensionHandshake::decode(&payload);
                assert!(!decoded.m.contains_key(UT_PEX));
                saw = true;
                break;
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert!(saw, "did not receive extension handshake");
    seeder.shutdown();
}

#[test]
fn compact_encoder_is_shared_with_pex_payload() {
    let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
    let encoded = encode_compact_v4(&[addr]);
    assert_eq!(encoded, common::compact_peers(&[addr]));
}

// Keep test_session linked so a default public session still registers PEX.
#[tokio::test]
async fn default_test_session_registers_ut_pex() {
    let session = test_session(None).await;
    assert!(session.extensions().contains(UT_PEX));
    session.shutdown();
}
