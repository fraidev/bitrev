mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bit_rev::dht::{DhtHandle, DhtOptions, PeerSink};
use bit_rev::discovery::DiscoverySource;
use bit_rev::file::from_filename;
use bit_rev::session::{AddTorrentOptions, Session, SessionOptions};
use common::{
    add_download, wait_for_completion, SeederConfig, SeederPeer, TorrentFixture, DOWNLOAD_TIMEOUT,
    LISTEN_TIMEOUT,
};
use tokio_util::sync::CancellationToken;

fn noop_sink() -> PeerSink {
    Arc::new(|_, _| {})
}

type FoundPeers = Vec<([u8; 20], Vec<std::net::SocketAddr>)>;

fn collecting_sink() -> (PeerSink, Arc<Mutex<FoundPeers>>) {
    let found = Arc::new(Mutex::new(Vec::new()));
    let sink_found = found.clone();
    let sink: PeerSink = Arc::new(move |ih, addrs| {
        sink_found.lock().unwrap().push((ih, addrs));
    });
    (sink, found)
}

fn spawn_node(bootstrap: Vec<String>, sink: PeerSink) -> DhtHandle {
    DhtHandle::spawn(
        DhtOptions {
            enabled: true,
            port: 0,
            bootstrap_nodes: bootstrap,
        },
        None,
        sink,
        CancellationToken::new(),
    )
    .expect("start dht")
}

async fn wait_until<F>(timeout: Duration, mut pred: F)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while !pred() {
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for condition");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn two_nodes_exchange_queries_and_lookup_finds_announce() {
    let a = spawn_node(vec![], noop_sink());
    let b_bootstrap = vec![format!("127.0.0.1:{}", a.local_addr().port())];
    let (sink, found) = collecting_sink();
    let b = spawn_node(b_bootstrap, sink);

    wait_until(Duration::from_secs(3), || {
        a.stats().nodes >= 1 && b.stats().nodes >= 1
    })
    .await;

    let info_hash = [0xABu8; 20];
    a.add_torrent(info_hash, 51413, true);
    tokio::time::sleep(Duration::from_millis(400)).await;
    b.add_torrent(info_hash, 0, true);

    wait_until(Duration::from_secs(3), || {
        found
            .lock()
            .unwrap()
            .iter()
            .any(|(ih, addrs)| *ih == info_hash && addrs.iter().any(|addr| addr.port() == 51413))
    })
    .await;

    a.shutdown();
    b.shutdown();
}

#[tokio::test]
async fn private_torrent_never_registers_with_dht() {
    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir: None,
        dht: DhtOptions {
            enabled: true,
            port: 0,
            bootstrap_nodes: vec![],
        },
        ..SessionOptions::default()
    });
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("listen");

    let meta = from_filename(&format!(
        "{}/tests/fixtures/private.torrent",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("private fixture");
    let dir = tempfile::tempdir().unwrap();
    session
        .add_torrent(AddTorrentOptions::from(meta.clone()).output_dir(dir.path().join("out")))
        .await
        .expect("add private");

    tokio::time::sleep(Duration::from_millis(100)).await;
    let dht = session.dht().expect("dht started");
    assert!(
        dht.watched().is_empty(),
        "private torrent must not be tracked by DHT"
    );
    assert_eq!(
        session.add_peers(&meta.info_hash, DiscoverySource::Dht, vec![]),
        0
    );
    session.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_peers_dht_source_completes_download_without_tracker() {
    let fixture = Arc::new(TorrentFixture::single(64 * 1024, 32 * 1024, 0xD07_0005));
    let seeder = SeederPeer::start(fixture.clone(), SeederConfig::all_pieces()).await;
    assert!(fixture.torrent_meta.torrent_file.announce.is_none());

    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir: None,
        dht: DhtOptions {
            enabled: false,
            port: 0,
            bootstrap_nodes: vec![],
        },
        ..SessionOptions::default()
    });
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("listen");

    let download_dir = common::unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let added = add_download(&session, fixture.torrent_meta.clone(), output.clone()).await;
    let n = session.add_peers(
        &fixture.torrent_meta.info_hash,
        DiscoverySource::Dht,
        vec![seeder.addr],
    );
    assert_eq!(n, 1, "dht add_peers should connect the seeder");
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    session.shutdown();
    fixture.assert_output_matches(&output);
}

#[tokio::test]
async fn public_torrent_is_watched_by_dht() {
    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir: None,
        dht: DhtOptions {
            enabled: true,
            port: 0,
            bootstrap_nodes: vec![],
        },
        ..SessionOptions::default()
    });
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("listen");

    let fixture = TorrentFixture::single(16 * 1024, 16 * 1024, 7);
    let dir = tempfile::tempdir().unwrap();
    let added = add_download(
        &session,
        fixture.torrent_meta.clone(),
        fixture.session_output(dir.path()),
    )
    .await;
    wait_until(Duration::from_secs(2), || {
        session
            .dht()
            .map(|d| d.watched().contains(&added.torrent.info_hash))
            .unwrap_or(false)
    })
    .await;
    assert!(session.dht_stats().is_some());
    session.shutdown();
}
