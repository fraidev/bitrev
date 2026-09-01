mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    add_download, test_session, unique_temp_dir, wait_for_completion, FileSpec, HttpAnnounceBody,
    MockHttpTracker, MockUdpTracker, SeederConfig, SeederPeer, TorrentFixture, UdpAnnounceBody,
    BLOCK_SIZE, DEFAULT_PIECE_LENGTH, DOWNLOAD_TIMEOUT,
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
        added.already_have.len(),
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
        added.already_have.len(),
        DOWNLOAD_TIMEOUT,
    )
    .await;
    session.shutdown();
    fixture.assert_output_matches(&output);
    assert!(corrupt.blocks_sent() > 0);
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
        added.already_have.len(),
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
        added.already_have.len(),
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
    const RSS_BOUND: u64 = 400 * 1024 * 1024;

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
        added.already_have.len(),
        Duration::from_secs(180),
    )
    .await;
    session.shutdown();
    fixture.assert_output_matches(&output);

    if let Some(rss) = common::peak_rss_bytes() {
        assert!(rss < RSS_BOUND, "peak RSS {rss} exceeded bound {RSS_BOUND}");
    }
}
