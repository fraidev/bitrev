mod common;

use std::net::{Ipv4Addr, SocketAddr};

use bit_rev::dht::DhtOptions;
use bit_rev::lsd;
use bit_rev::mse::EncryptionPolicy;
use bit_rev::session::{AddTorrentOptions, Session, SessionOptions};
use bit_rev::utp::UtpOptions;
use common::{unique_temp_dir, TorrentFixture, LISTEN_TIMEOUT};

fn lpd_session() -> Session {
    Session::with_options(SessionOptions {
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
    })
}

fn announce(port: u16, info_hash: &[u8; 20], cookie: &str) -> Vec<u8> {
    lsd::encode_announce(port, &[*info_hash], cookie).expect("announce")
}

#[tokio::test]
async fn injected_announce_lands_in_peer_states() {
    let session = lpd_session();
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("listen");
    let fixture = TorrentFixture::single(16 * 1024, 16 * 1024, 0x1D40);
    let dir = unique_temp_dir();
    session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(fixture.session_output(dir.path())),
        )
        .await
        .expect("add");
    assert!(session
        .lpd_announce_hashes()
        .contains(&fixture.torrent_meta.info_hash));

    let src = SocketAddr::from((Ipv4Addr::new(192, 168, 8, 40), 40000));
    let peer = SocketAddr::from((Ipv4Addr::new(192, 168, 8, 40), 6881));
    let datagram = announce(6881, &fixture.torrent_meta.info_hash, "neighbor");
    let added_peers = session.handle_lpd_datagram(src, &datagram);
    assert_eq!(added_peers, 1);
    let states = session
        .torrent_session(&fixture.torrent_meta.info_hash)
        .expect("torrent")
        .peer_states
        .clone();
    assert!(states.states.contains_key(&peer));
}

#[tokio::test]
async fn private_torrent_is_not_announced_and_ignores_matches() {
    let session = lpd_session();
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("listen");
    let fixture = TorrentFixture::builder()
        .single_file("payload.bin", 16 * 1024)
        .piece_length(16 * 1024)
        .seed(0x1D41)
        .private(true)
        .build();
    let dir = unique_temp_dir();
    session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(fixture.session_output(dir.path())),
        )
        .await
        .expect("add private");
    assert!(fixture.torrent_meta.torrent_file.info.is_private());
    assert!(!session
        .lpd_announce_hashes()
        .contains(&fixture.torrent_meta.info_hash));

    let src = SocketAddr::from((Ipv4Addr::new(192, 168, 8, 41), 40000));
    let peer = SocketAddr::from((Ipv4Addr::new(192, 168, 8, 41), 6881));
    let datagram = announce(6881, &fixture.torrent_meta.info_hash, "neighbor");
    assert_eq!(session.handle_lpd_datagram(src, &datagram), 0);
    let states = session
        .torrent_session(&fixture.torrent_meta.info_hash)
        .expect("torrent")
        .peer_states
        .clone();
    assert!(!states.states.contains_key(&peer));
}
