mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bit_rev::mse::EncryptionPolicy;
use bit_rev::resume::{self, ResumeData, RESUME_VERSION};
use bit_rev::session::{AddTorrentOptions, Session, SessionOptions};
use serde_bytes::ByteBuf;

use common::{
    add_download, test_session, unique_temp_dir, wait_for_completion, FileSpec, HttpAnnounceBody,
    MockHttpTracker, MockUdpTracker, SeederConfig, SeederPeer, TorrentFixture, UdpAnnounceBody,
    BLOCK_SIZE, DEFAULT_PIECE_LENGTH, DOWNLOAD_TIMEOUT, LISTEN_TIMEOUT,
};

const FOUR_MIB: u64 = 4 * 1024 * 1024;

#[test]
fn fixture_is_deterministic() {
    let a = TorrentFixture::single(64 * 1024, DEFAULT_PIECE_LENGTH, 42);
    let b = TorrentFixture::single(64 * 1024, DEFAULT_PIECE_LENGTH, 42);
    assert_eq!(a.torrent_meta.info_hash, b.torrent_meta.info_hash);
    assert_eq!(a.piece_hashes, b.piece_hashes);
    assert_eq!(a.payload_bytes(), b.payload_bytes());
}

fn partition_pieces(piece_count: u32, buckets: usize) -> Vec<Vec<u32>> {
    let mut out = vec![Vec::new(); buckets];
    for index in 0..piece_count {
        out[index as usize % buckets].push(index);
    }
    out
}

fn unique_peer_id(tag: u8) -> [u8; 20] {
    let mut id = *b"-SDIT01-............";
    id[19] = tag;
    id
}

async fn start_seeders(
    fixture: &Arc<TorrentFixture>,
    configs: Vec<SeederConfig>,
) -> Vec<SeederPeer> {
    let mut seeders = Vec::with_capacity(configs.len());
    for config in configs {
        seeders.push(SeederPeer::start(fixture.clone(), config).await);
    }
    seeders
}

async fn download_via_http(
    fixture: &TorrentFixture,
    seeders: &[SeederPeer],
    output_parent: &std::path::Path,
) {
    let peers: Vec<_> = seeders.iter().map(|s| s.addr).collect();
    let tracker = MockHttpTracker::start(vec![HttpAnnounceBody::peers(1800, peers)]).await;
    let meta = fixture.meta_with_trackers(Some(tracker.url.clone()), None);

    let session = test_session(None).await;
    let output = fixture.session_output(output_parent);
    let added = add_download(&session, meta, output.clone()).await;
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    tracker.wait_requests(1, Duration::from_secs(10)).await;
    session.shutdown();
    fixture.assert_output_matches(&output);
    let requests = tracker.requests();
    assert!(
        !requests.is_empty(),
        "leecher never announced to the mock tracker"
    );
    assert!(
        requests
            .iter()
            .any(|r| r.query_param("info_hash").is_some()),
        "announce is missing info_hash"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_single_file_three_seeders() {
    let fixture = Arc::new(TorrentFixture::single(
        FOUR_MIB,
        DEFAULT_PIECE_LENGTH,
        0x5104_E2E1,
    ));
    let seeders = start_seeders(
        &fixture,
        vec![
            SeederConfig::all_pieces().peer_id(unique_peer_id(1)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(3)),
        ],
    )
    .await;

    let download_dir = unique_temp_dir();
    download_via_http(&fixture, &seeders, download_dir.path()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_multi_file_layout_and_bytes() {
    let fixture = Arc::new(
        TorrentFixture::builder()
            .name("bundle")
            .seed(0x4D_F1_1E_01)
            .piece_length(DEFAULT_PIECE_LENGTH)
            .files(vec![
                FileSpec::new(["tiny.txt"], 1_000),
                FileSpec::new(["a.bin"], 15_000),
                FileSpec::new(["b.bin"], 16_768),
                FileSpec::new(["nested", "rest.bin"], 40_000),
            ])
            .build(),
    );
    assert!(
        fixture.files[0].length < u64::from(fixture.piece_length),
        "first file must be smaller than a piece"
    );
    let torrent = fixture.torrent();
    let piece0 = bit_rev::utils::map_piece_to_files(&torrent, 0);
    assert_eq!(
        piece0.len(),
        3,
        "piece 0 should span the first three files, got {piece0:?}"
    );

    let seeders = start_seeders(
        &fixture,
        vec![
            SeederConfig::all_pieces().peer_id(unique_peer_id(1)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(3)),
        ],
    )
    .await;

    let download_dir = unique_temp_dir();
    download_via_http(&fixture, &seeders, download_dir.path()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_peer_disjoint_pieces() {
    let fixture = Arc::new(TorrentFixture::single(
        FOUR_MIB,
        DEFAULT_PIECE_LENGTH,
        0xC00D_D100,
    ));
    let parts = partition_pieces(fixture.piece_count() as u32, 3);
    assert!(parts.iter().all(|p| !p.is_empty()));

    let seeders = start_seeders(
        &fixture,
        vec![
            SeederConfig::with_pieces(parts[0].clone()).peer_id(unique_peer_id(1)),
            SeederConfig::with_pieces(parts[1].clone()).peer_id(unique_peer_id(2)),
            SeederConfig::with_pieces(parts[2].clone()).peer_id(unique_peer_id(3)),
        ],
    )
    .await;

    let download_dir = unique_temp_dir();
    download_via_http(&fixture, &seeders, download_dir.path()).await;

    for (i, seeder) in seeders.iter().enumerate() {
        assert!(
            seeder.blocks_sent() > 0,
            "seeder {i} served zero blocks (partition {:?})",
            parts[i]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_piece_is_refetched() {
    let fixture = Arc::new(TorrentFixture::single(
        512 * 1024,
        DEFAULT_PIECE_LENGTH,
        0xC044_0700,
    ));
    let piece0_blocks = u64::from(fixture.piece_len(0).div_ceil(BLOCK_SIZE));
    let corrupt = SeederPeer::start(
        fixture.clone(),
        SeederConfig::with_pieces([0])
            .peer_id(unique_peer_id(1))
            .corrupt(0)
            .disconnect_after_blocks(piece0_blocks),
    )
    .await;
    let honest = SeederPeer::start(
        fixture.clone(),
        SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
    )
    .await;

    let tracker =
        MockHttpTracker::start(vec![HttpAnnounceBody::peers(1800, vec![corrupt.addr])]).await;
    let meta = fixture.meta_with_trackers(Some(tracker.url.clone()), None);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let output = fixture.session_output(download_dir.path());
    let added = add_download(&session, meta, output.clone()).await;

    corrupt
        .wait_blocks_sent(piece0_blocks, Duration::from_secs(10))
        .await;
    assert!(
        session.connect_peer(&fixture.torrent_meta.info_hash, honest.addr),
        "failed to connect honest seeder"
    );
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    session.shutdown();
    fixture.assert_output_matches(&output);
    assert!(corrupt.blocks_sent() > 0);
    assert!(honest.blocks_sent() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn corrupt_seeder_is_banned_and_download_completes() {
    let fixture = Arc::new(TorrentFixture::single(
        256 * 1024,
        DEFAULT_PIECE_LENGTH,
        0xBA11_0001,
    ));
    let corrupt = SeederPeer::start(
        fixture.clone(),
        SeederConfig::with_pieces([0])
            .peer_id(unique_peer_id(1))
            .corrupt(0),
    )
    .await;
    let honest = SeederPeer::start(
        fixture.clone(),
        SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
    )
    .await;

    let tracker =
        MockHttpTracker::start(vec![HttpAnnounceBody::peers(1800, vec![corrupt.addr])]).await;
    let meta = fixture.meta_with_trackers(Some(tracker.url.clone()), None);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let output = fixture.session_output(download_dir.path());
    let added = add_download(&session, meta, output.clone()).await;

    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(torrent) = session.torrent_session(&fixture.torrent_meta.info_hash) {
                if torrent.peer_states.is_banned(corrupt.addr) {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("corrupt seeder should be banned after hash failure");

    assert!(
        session.connect_peer(&fixture.torrent_meta.info_hash, honest.addr),
        "failed to connect honest seeder"
    );
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;

    let torrent = session
        .torrent_session(&fixture.torrent_meta.info_hash)
        .expect("torrent session");
    assert!(
        torrent.peer_states.is_banned(corrupt.addr),
        "corrupt seeder should stay banned"
    );
    assert!(torrent.peer_states.banned_count() >= 1);
    assert!(!torrent.peer_states.is_banned(honest.addr));

    session.shutdown();
    fixture.assert_output_matches(&output);
    assert!(honest.blocks_sent() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_mid_piece_is_retried() {
    let fixture = Arc::new(TorrentFixture::single(
        512 * 1024,
        DEFAULT_PIECE_LENGTH,
        0xD15C_0001,
    ));
    assert!(
        fixture.piece_len(0) > BLOCK_SIZE,
        "need a multi-block piece to drop mid-piece"
    );
    let flaky = SeederPeer::start(
        fixture.clone(),
        SeederConfig::with_pieces([0])
            .peer_id(unique_peer_id(1))
            .disconnect_after_blocks(1),
    )
    .await;
    let honest = SeederPeer::start(
        fixture.clone(),
        SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
    )
    .await;

    let tracker =
        MockHttpTracker::start(vec![HttpAnnounceBody::peers(1800, vec![flaky.addr])]).await;
    let meta = fixture.meta_with_trackers(Some(tracker.url.clone()), None);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let output = fixture.session_output(download_dir.path());
    let added = add_download(&session, meta, output.clone()).await;

    flaky.wait_blocks_sent(1, Duration::from_secs(10)).await;
    assert!(
        session.connect_peer(&fixture.torrent_meta.info_hash, honest.addr),
        "failed to connect honest seeder"
    );
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    session.shutdown();
    fixture.assert_output_matches(&output);
    assert_eq!(flaky.blocks_sent(), 1);
    assert!(honest.blocks_sent() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tracker_failure_then_second_tracker_succeeds() {
    let fixture = Arc::new(TorrentFixture::single(
        256 * 1024,
        DEFAULT_PIECE_LENGTH,
        0xFA11_0002,
    ));
    let seeders = start_seeders(
        &fixture,
        vec![SeederConfig::all_pieces().peer_id(unique_peer_id(1))],
    )
    .await;

    let failing = MockHttpTracker::start(vec![HttpAnnounceBody::Failure(
        "unregistered torrent".into(),
    )])
    .await;
    let udp =
        MockUdpTracker::start(vec![UdpAnnounceBody::peers(1800, vec![seeders[0].addr])]).await;

    let meta = fixture.meta_with_trackers(
        Some(failing.url.clone()),
        Some(vec![vec![failing.url.clone()], vec![udp.url.clone()]]),
    );

    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let output = fixture.session_output(download_dir.path());
    let added = add_download(&session, meta, output.clone()).await;
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    failing.wait_requests(1, Duration::from_secs(10)).await;
    udp.wait_announces(1, Duration::from_secs(10)).await;
    session.shutdown();
    fixture.assert_output_matches(&output);
    assert_eq!(
        udp.announces()[0].info_hash,
        fixture.torrent_meta.info_hash,
        "UDP announce info_hash mismatch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_peer_does_not_block_download() {
    let fixture = Arc::new(TorrentFixture::single(
        1024 * 1024,
        DEFAULT_PIECE_LENGTH,
        0x5100_0001,
    ));
    let seeders = start_seeders(
        &fixture,
        vec![
            SeederConfig::all_pieces()
                .peer_id(unique_peer_id(1))
                .latency(Duration::from_millis(200)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(3)),
        ],
    )
    .await;

    let download_dir = unique_temp_dir();
    let started = Instant::now();
    download_via_http(&fixture, &seeders, download_dir.path()).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "download took {elapsed:?}, possible head-of-line blocking on the slow seeder"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "512 MiB fixture; run with --ignored"]
async fn large_file_stays_memory_bounded() {
    const LARGE: u64 = 512 * 1024 * 1024;
    const RSS_BOUND: u64 = LARGE / 8 + 64 * 1024 * 1024;

    let fixture = Arc::new(
        TorrentFixture::builder()
            .single_file("large.bin", LARGE)
            .piece_length(256 * 1024)
            .seed(0x1A4E_0001)
            .keep_payload(false)
            .build(),
    );
    let seeders = start_seeders(
        &fixture,
        vec![SeederConfig::all_pieces().peer_id(unique_peer_id(1))],
    )
    .await;

    let download_dir = unique_temp_dir();
    let tracker =
        MockHttpTracker::start(vec![HttpAnnounceBody::peers(1800, vec![seeders[0].addr])]).await;
    let meta = fixture.meta_with_trackers(Some(tracker.url.clone()), None);

    let session = test_session(None).await;
    let output = fixture.session_output(download_dir.path());
    let added = add_download(&session, meta, output.clone()).await;
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        Duration::from_secs(180),
    )
    .await;
    session.shutdown();
    let rss = common::peak_rss_bytes();
    fixture.assert_output_matches(&output);

    if let Some(rss) = rss {
        assert!(rss < RSS_BOUND, "peak RSS {rss} exceeded bound {RSS_BOUND}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recheck_does_not_stall_other_download() {
    const BIG: u64 = 64 * 1024 * 1024;
    let big = TorrentFixture::builder()
        .single_file("big.bin", BIG)
        .piece_length(256 * 1024)
        .seed(0x4EC0_0001)
        .keep_payload(false)
        .build();
    let small = Arc::new(TorrentFixture::single(
        64 * 1024,
        DEFAULT_PIECE_LENGTH,
        0x4EC0_0002,
    ));
    let small_seeders = start_seeders(
        &small,
        vec![SeederConfig::all_pieces().peer_id(unique_peer_id(9))],
    )
    .await;

    let state_dir = unique_temp_dir();
    let torrent = big.torrent();
    let output = big.files[0].disk_path.clone();
    let layout = resume::collect_file_layout(&torrent, &output);
    let resume_data = ResumeData {
        version: RESUME_VERSION,
        info_hash: ByteBuf::from(big.torrent_meta.info_hash.to_vec()),
        bitfield: ByteBuf::from(vec![0u8; torrent.piece_hashes.len().div_ceil(8)]),
        output_dir: output.to_string_lossy().into_owned(),
        files: layout,
        uploaded: 0,
        downloaded: 0,
        torrent_path: String::new(),
        paused: 0,
        added_at: 1,
        completed_at: 0,
    };
    resume::save(
        &resume::resume_path(state_dir.path(), &big.torrent_meta.info_hash),
        &resume_data,
    )
    .unwrap();

    let recheck_session = test_session(Some(state_dir.path().to_path_buf())).await;
    let download_session = test_session(None).await;

    let tracker = MockHttpTracker::start(vec![HttpAnnounceBody::peers(
        1800,
        vec![small_seeders[0].addr],
    )])
    .await;
    let small_meta = small.meta_with_trackers(Some(tracker.url.clone()), None);
    let download_dir = unique_temp_dir();
    let small_output = small.session_output(download_dir.path());

    let recheck = async {
        recheck_session
            .add_torrent(
                AddTorrentOptions::from(big.torrent_meta.clone())
                    .output_dir(output)
                    .verify(true),
            )
            .await
            .expect("recheck add")
    };
    let download = async {
        let started = Instant::now();
        let added = add_download(&download_session, small_meta, small_output.clone()).await;
        wait_for_completion(
            &added.pr_rx,
            &added.torrent,
            &added.already_have,
            Duration::from_secs(8),
        )
        .await;
        started.elapsed()
    };

    let (added, dl_time) = tokio::join!(recheck, download);
    assert_eq!(
        added.resume_status,
        bit_rev::session::ResumeStatus::SlowPath
    );
    assert!(
        dl_time < Duration::from_secs(8),
        "small download stalled during recheck: {dl_time:?}"
    );
    recheck_session.shutdown();
    download_session.shutdown();
    small.assert_output_matches(&small_output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rarest_first_unique_pieces_before_common() {
    let fixture = Arc::new(TorrentFixture::single(
        12 * u64::from(DEFAULT_PIECE_LENGTH),
        DEFAULT_PIECE_LENGTH,
        0x4A4E_0001,
    ));
    let piece_count = fixture.piece_count() as u32;
    assert_eq!(piece_count, 12);

    let unique_a = vec![0u32, 1, 2];
    let unique_b = vec![3u32, 4, 5];
    let unique_c = vec![6u32, 7, 8];
    let common = [9u32, 10, 11];

    let mut a_set = unique_a.clone();
    a_set.extend_from_slice(&common);
    let mut b_set = unique_b.clone();
    b_set.extend_from_slice(&common);
    let mut c_set = unique_c.clone();
    c_set.extend_from_slice(&common);

    let seeders = start_seeders(
        &fixture,
        vec![
            SeederConfig::with_pieces(a_set).peer_id(unique_peer_id(1)),
            SeederConfig::with_pieces(b_set).peer_id(unique_peer_id(2)),
            SeederConfig::with_pieces(c_set).peer_id(unique_peer_id(3)),
            SeederConfig::all_pieces()
                .peer_id(unique_peer_id(4))
                .latency(Duration::from_millis(200)),
        ],
    )
    .await;

    let download_dir = unique_temp_dir();
    download_via_http(&fixture, &seeders, download_dir.path()).await;

    let expected = [&unique_a, &unique_b, &unique_c];
    for (i, unique) in expected.iter().enumerate() {
        let requested = seeders[i].requested_pieces();
        for piece in unique.iter() {
            assert!(
                requested.contains(piece),
                "seeder {i} was never asked for unique piece {piece}, got {requested:?}"
            );
        }
        assert!(
            seeders[i].blocks_sent() > 0,
            "unique seeder {i} sent nothing"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn endgame_duplicate_bytes_under_two_pieces() {
    let fixture = Arc::new(TorrentFixture::single(
        4 * u64::from(DEFAULT_PIECE_LENGTH),
        DEFAULT_PIECE_LENGTH,
        0xE2D6_0001,
    ));
    let seeders = start_seeders(
        &fixture,
        vec![
            SeederConfig::all_pieces().peer_id(unique_peer_id(1)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(3)),
        ],
    )
    .await;

    let peers: Vec<_> = seeders.iter().map(|s| s.addr).collect();
    let tracker = MockHttpTracker::start(vec![HttpAnnounceBody::peers(1800, peers)]).await;
    let meta = fixture.meta_with_trackers(Some(tracker.url.clone()), None);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let output = fixture.session_output(download_dir.path());
    let added = add_download(&session, meta, output.clone()).await;
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;

    let torrent = session
        .torrent_session(&fixture.torrent_meta.info_hash)
        .expect("torrent session");
    let dup = torrent.downloaded_state.duplicate_bytes();
    let bound = 2 * u64::from(DEFAULT_PIECE_LENGTH);
    assert!(
        dup < bound,
        "endgame duplicate bytes {dup} exceeded bound {bound}"
    );
    fixture.assert_output_matches(&output);
    session.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_unchoke_seeder_does_not_stall_download() {
    let fixture = Arc::new(TorrentFixture::single(
        2 * u64::from(DEFAULT_PIECE_LENGTH),
        DEFAULT_PIECE_LENGTH,
        0x5B00_0001,
    ));
    let seeders = start_seeders(
        &fixture,
        vec![
            SeederConfig::all_pieces()
                .peer_id(unique_peer_id(1))
                .never_unchoke(),
            SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
            SeederConfig::all_pieces().peer_id(unique_peer_id(3)),
        ],
    )
    .await;

    let download_dir = unique_temp_dir();
    let started = Instant::now();
    download_via_http(&fixture, &seeders, download_dir.path()).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "download took {elapsed:?}, never-unchoke seeder stalled the swarm"
    );
    assert_eq!(seeders[0].blocks_sent(), 0);
    assert!(seeders[1].blocks_sent() + seeders[2].blocks_sent() > 0);
}

fn loopback_addr(addr: SocketAddr) -> SocketAddr {
    if addr.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
    } else {
        addr
    }
}

async fn session_with_encryption(encryption: EncryptionPolicy) -> Session {
    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir: None,
        encryption,
        dht: bit_rev::session::DhtOptions {
            enabled: false,
            ..bit_rev::session::DhtOptions::default()
        },
        ..SessionOptions::default()
    });
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("session listen timeout");
    session
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_require_encrypted_two_sessions() {
    let fixture = Arc::new(TorrentFixture::single(
        64 * 1024,
        DEFAULT_PIECE_LENGTH,
        0xE5E7_0007,
    ));

    let seed_dir = unique_temp_dir();
    let seed_path = fixture.session_output(seed_dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &seed_path).expect("copy seed payload");

    let seeder = session_with_encryption(EncryptionPolicy::RequireEncrypted).await;
    seeder
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(seed_path)
                .seed(true),
        )
        .await
        .expect("add seeder");
    let seeder_addr = loopback_addr(seeder.wait_listening().await);

    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let leecher = session_with_encryption(EncryptionPolicy::RequireEncrypted).await;
    let added = add_download(&leecher, fixture.torrent_meta.clone(), output.clone()).await;
    assert!(
        leecher.connect_peer(&fixture.torrent_meta.info_hash, seeder_addr),
        "leecher should dial the seeder"
    );
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    fixture.assert_output_matches(&output);

    let leecher_torrent = leecher
        .torrent_session(&fixture.torrent_meta.info_hash)
        .expect("leecher torrent");
    assert!(
        leecher_torrent
            .peer_states
            .states
            .iter()
            .any(|entry| entry.encrypted),
        "require_encrypted download should mark the peer as encrypted"
    );

    leecher.shutdown();
    seeder.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_plaintext_leecher_joins_prefer_encrypted_seeder() {
    let fixture = Arc::new(TorrentFixture::single(
        32 * 1024,
        DEFAULT_PIECE_LENGTH,
        0xA07C_0007,
    ));

    let seed_dir = unique_temp_dir();
    let seed_path = fixture.session_output(seed_dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &seed_path).expect("copy seed payload");

    let seeder = session_with_encryption(EncryptionPolicy::PreferEncrypted).await;
    seeder
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(seed_path)
                .seed(true),
        )
        .await
        .expect("add seeder");
    let seeder_addr = loopback_addr(seeder.wait_listening().await);

    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let leecher = session_with_encryption(EncryptionPolicy::Disabled).await;
    let added = add_download(&leecher, fixture.torrent_meta.clone(), output.clone()).await;
    assert!(leecher.connect_peer(&fixture.torrent_meta.info_hash, seeder_addr));
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    fixture.assert_output_matches(&output);

    leecher.shutdown();
    seeder.shutdown();
}

async fn session_with_utp() -> Session {
    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir: None,
        encryption: EncryptionPolicy::Disabled,
        dht: bit_rev::session::DhtOptions {
            enabled: false,
            ..bit_rev::session::DhtOptions::default()
        },
        utp: bit_rev::session::UtpOptions {
            enabled: true,
            port: 0,
        },
        ..SessionOptions::default()
    });
    tokio::time::timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("session listen timeout");
    session
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2e_utp_two_sessions_through_lossy_relay() {
    let fixture = Arc::new(TorrentFixture::single(
        64 * 1024,
        DEFAULT_PIECE_LENGTH,
        0x5570_0005,
    ));

    let seed_dir = unique_temp_dir();
    let seed_path = fixture.session_output(seed_dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &seed_path).expect("copy seed payload");

    let seeder = session_with_utp().await;
    seeder
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(seed_path)
                .seed(true),
        )
        .await
        .expect("add seeder");
    let _ = seeder.wait_listening().await;
    let seeder_utp = seeder.utp_local_addr().expect("seeder uTP socket");

    let relay = bit_rev::utp::LossyRelay::bind(bit_rev::utp::RelayConfig::lossy(0.05))
        .await
        .expect("lossy relay");
    relay.set_backend(seeder_utp);

    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let leecher = session_with_utp().await;
    let added = add_download(&leecher, fixture.torrent_meta.clone(), output.clone()).await;
    assert!(
        leecher.connect_peer(&fixture.torrent_meta.info_hash, relay.local_addr()),
        "leecher should dial the seeder through the uTP relay"
    );
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        Duration::from_secs(60),
    )
    .await;
    fixture.assert_output_matches(&output);

    let leecher_torrent = leecher
        .torrent_session(&fixture.torrent_meta.info_hash)
        .expect("leecher torrent");
    assert!(
        leecher_torrent
            .peer_states
            .states
            .iter()
            .any(|entry| entry.utp),
        "lossy uTP download should mark the peer as utp"
    );

    leecher.shutdown();
    seeder.shutdown();
}
