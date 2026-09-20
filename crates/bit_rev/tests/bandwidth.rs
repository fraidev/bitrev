mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bit_rev::mse::EncryptionPolicy;
use bit_rev::session::{
    AddTorrentOptions, Session, SessionOptions, TorrentState, DEFAULT_QUEUE_SLOW_WINDOW,
};
use tokio::time::timeout;

use common::{
    add_download, unique_temp_dir, wait_for_completion, SeederConfig, SeederPeer, TorrentFixture,
    DEFAULT_PIECE_LENGTH, DOWNLOAD_TIMEOUT, LISTEN_TIMEOUT,
};

const DOWNLOAD_LIMIT: u64 = 256 * 1024;
const UPLOAD_LIMIT: u64 = 128 * 1024;
const PAYLOAD: u64 = 512 * 1024;

fn unique_peer_id(tag: u8) -> [u8; 20] {
    let mut id = *b"-SDIT38-............";
    id[19] = tag;
    id
}

fn named_fixture(name: &str, len: u64, seed: u64) -> TorrentFixture {
    TorrentFixture::builder()
        .single_file(name, len)
        .piece_length(DEFAULT_PIECE_LENGTH)
        .seed(seed)
        .build()
}

async fn session_with(mut opts: SessionOptions) -> Session {
    if opts.state_dir == SessionOptions::default().state_dir {
        opts.state_dir = None;
    }
    opts.listen_port = 0;
    opts.encryption = EncryptionPolicy::Disabled;
    let session = Session::with_options(opts);
    timeout(LISTEN_TIMEOUT, session.wait_listening())
        .await
        .expect("session listen timeout");
    session
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_limit_caps_transfer_stats_rate() {
    let fixture = Arc::new(named_fixture("dl-limit.bin", PAYLOAD, 0x3801));
    let seeder = SeederPeer::start(
        fixture.clone(),
        SeederConfig::all_pieces().peer_id(unique_peer_id(1)),
    )
    .await;
    let download_dir = unique_temp_dir();
    let session = session_with(SessionOptions {
        download_limit: DOWNLOAD_LIMIT,
        max_active_downloads: 0,
        ..SessionOptions::default()
    })
    .await;
    let stats = session.transfer_stats();
    assert_eq!(stats.download_limit, DOWNLOAD_LIMIT);
    assert!(!stats.alt_mode);

    let added = add_download(
        &session,
        fixture.torrent_meta.clone(),
        fixture.session_output(download_dir.path()),
    )
    .await;
    assert!(session.connect_peer(&fixture.torrent_meta.info_hash, seeder.addr));
    let start = Instant::now();
    let mut max_rate = 0u64;
    let deadline = tokio::time::Instant::now() + DOWNLOAD_TIMEOUT;
    loop {
        let stats = session.transfer_stats();
        max_rate = max_rate.max(stats.download_rate);
        if session
            .snapshot(added.id)
            .is_some_and(|s| s.state == TorrentState::Seeding || s.progress >= 1.0)
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            wait_for_completion(
                &added.pr_rx,
                &added.torrent,
                &added.already_have,
                Duration::from_secs(1),
            )
            .await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let elapsed = start.elapsed();
    session.shutdown();

    assert!(
        elapsed >= Duration::from_millis(1200),
        "limited download finished too fast: {elapsed:?}"
    );
    let cap = DOWNLOAD_LIMIT + DOWNLOAD_LIMIT / 10;
    assert!(
        max_rate <= cap,
        "download_rate {max_rate} exceeded cap {cap} (limit {DOWNLOAD_LIMIT})"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_limit_caps_seeder_blocks_sent() {
    let fixture = Arc::new(named_fixture("ul-limit.bin", PAYLOAD, 0x3802));
    let seed_dir = unique_temp_dir();
    let seed_path = fixture.session_output(seed_dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &seed_path).unwrap();

    let seeder = session_with(SessionOptions {
        upload_limit: UPLOAD_LIMIT,
        max_active_uploads: 0,
        max_active_downloads: 0,
        ..SessionOptions::default()
    })
    .await;
    seeder
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(seed_path)
                .seed(true),
        )
        .await
        .unwrap();
    let seed_addr = {
        let addr = seeder.wait_listening().await;
        if addr.ip().is_unspecified() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
        } else {
            addr
        }
    };

    let download_dir = unique_temp_dir();
    let leecher = session_with(SessionOptions {
        max_active_downloads: 0,
        ..SessionOptions::default()
    })
    .await;
    let added = add_download(
        &leecher,
        fixture.torrent_meta.clone(),
        fixture.session_output(download_dir.path()),
    )
    .await;
    assert!(leecher.connect_peer(&fixture.torrent_meta.info_hash, seed_addr));

    let window = Duration::from_secs(3);
    tokio::time::sleep(window).await;
    let uploaded = seeder.uploaded();
    let rate = uploaded as f64 / window.as_secs_f64();
    let cap = UPLOAD_LIMIT as f64 * 1.10;
    seeder.shutdown();
    leecher.shutdown();
    let _ = added;

    assert!(
        uploaded >= UPLOAD_LIMIT,
        "seeder should have uploaded, got {uploaded}"
    );
    assert!(
        rate <= cap,
        "upload rate {rate:.0} exceeded cap {cap:.0} (uploaded {uploaded} in 3s)"
    );
}

#[tokio::test]
async fn sixth_download_is_queued_until_a_slot_frees() {
    let session = session_with(SessionOptions {
        max_active_downloads: 5,
        dont_count_slow: false,
        ..SessionOptions::default()
    })
    .await;
    let download_dir = unique_temp_dir();
    let mut ids = Vec::new();
    for i in 0..6 {
        let fixture = named_fixture(&format!("q{i}.bin"), 64 * 1024, 0x3810 + i as u64);
        let added = add_download(
            &session,
            fixture.torrent_meta.clone(),
            fixture.session_output(download_dir.path()),
        )
        .await;
        ids.push(added.id);
    }
    let states: Vec<_> = ids
        .iter()
        .map(|id| session.snapshot(*id).unwrap().state)
        .collect();
    assert_eq!(
        states
            .iter()
            .filter(|s| **s == TorrentState::Queued)
            .count(),
        1,
        "expected one queued torrent, got {states:?}"
    );
    assert_eq!(states[5], TorrentState::Queued);

    session.remove_torrent(ids[0], false).unwrap();
    assert_ne!(
        session.snapshot(ids[5]).unwrap().state,
        TorrentState::Queued,
        "queued torrent should start after a slot frees"
    );
    session.shutdown();
}

#[tokio::test]
async fn force_start_bypasses_the_queue() {
    let session = session_with(SessionOptions {
        max_active_downloads: 5,
        dont_count_slow: false,
        ..SessionOptions::default()
    })
    .await;
    let download_dir = unique_temp_dir();
    for i in 0..5 {
        let fixture = named_fixture(&format!("fs{i}.bin"), 64 * 1024, 0x3820 + i as u64);
        add_download(
            &session,
            fixture.torrent_meta.clone(),
            fixture.session_output(download_dir.path()),
        )
        .await;
    }
    let sixth = named_fixture("fs5.bin", 64 * 1024, 0x3825);
    let added = add_download(
        &session,
        sixth.torrent_meta.clone(),
        sixth.session_output(download_dir.path()),
    )
    .await;
    assert_eq!(
        session.snapshot(added.id).unwrap().state,
        TorrentState::Queued
    );
    session.set_force_start(added.id, true).unwrap();
    assert_ne!(
        session.snapshot(added.id).unwrap().state,
        TorrentState::Queued
    );
    assert!(session.snapshot(added.id).unwrap().force_start);
    session.shutdown();
}

#[tokio::test]
async fn ratio_limit_pauses_after_synthesized_upload() {
    let fixture = named_fixture("ratio.bin", 64 * 1024, 0x3830);
    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    std::fs::copy(&fixture.files[0].disk_path, &output).unwrap();
    let session = session_with(SessionOptions {
        seed_ratio_limit: 1.0,
        max_active_uploads: 0,
        max_active_downloads: 0,
        ..SessionOptions::default()
    })
    .await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(output.clone())
                .seed(true)
                .ratio_limit(1.0),
        )
        .await
        .unwrap();
    assert_eq!(
        session.snapshot(added.id).unwrap().state,
        TorrentState::Seeding
    );
    let downloaded = session.snapshot(added.id).unwrap().downloaded.max(1);
    session
        .torrent_session(&added.id.0)
        .unwrap()
        .uploaded
        .store(downloaded, Ordering::Relaxed);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if session.snapshot(added.id).unwrap().state == TorrentState::Paused {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        session.snapshot(added.id).unwrap().state,
        TorrentState::Paused
    );
    assert!(output.exists(), "paused-at-limit torrent must stay on disk");
    session.shutdown();
}

#[tokio::test]
async fn dont_count_slow_frees_a_slot() {
    let session = session_with(SessionOptions {
        max_active_downloads: 5,
        dont_count_slow: true,
        queue_slow_window: Duration::from_millis(400),
        ..SessionOptions::default()
    })
    .await;
    let download_dir = unique_temp_dir();
    let mut ids = Vec::new();
    for i in 0..6 {
        let fixture = named_fixture(&format!("slow{i}.bin"), 64 * 1024, 0x3840 + i as u64);
        let added = add_download(
            &session,
            fixture.torrent_meta.clone(),
            fixture.session_output(download_dir.path()),
        )
        .await;
        ids.push(added.id);
    }
    assert_eq!(
        session.snapshot(ids[5]).unwrap().state,
        TorrentState::Queued
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline {
        if session.snapshot(ids[5]).unwrap().state != TorrentState::Queued {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_ne!(
        session.snapshot(ids[5]).unwrap().state,
        TorrentState::Queued,
        "slow active torrents should free a slot"
    );
    session.shutdown();
}

#[tokio::test]
async fn queue_and_force_start_survive_restart() {
    let root = unique_temp_dir();
    let state_dir = root.path().join("state");
    let first = session_with(SessionOptions {
        state_dir: Some(state_dir.clone()),
        max_active_downloads: 5,
        dont_count_slow: false,
        queue_slow_window: DEFAULT_QUEUE_SLOW_WINDOW,
        ..SessionOptions::default()
    })
    .await;
    let mut ids = Vec::new();
    for i in 0..6 {
        let fixture = named_fixture(&format!("rs{i}.bin"), 64 * 1024, 0x3850 + i as u64);
        let added = add_download(
            &first,
            fixture.torrent_meta.clone(),
            fixture.session_output(root.path()),
        )
        .await;
        ids.push(added.id);
    }
    first.set_force_start(ids[5], true).unwrap();
    first.set_queue_position(ids[5], 0).unwrap();
    first.flush_resume().await;
    let pos = first.snapshot(ids[5]).unwrap().queue_position;
    drop(first);

    let second = Session::open(SessionOptions {
        listen_port: 0,
        state_dir: Some(state_dir),
        encryption: EncryptionPolicy::Disabled,
        max_active_downloads: 5,
        dont_count_slow: false,
        ..SessionOptions::default()
    })
    .await
    .expect("open");
    let _ = timeout(LISTEN_TIMEOUT, second.wait_listening()).await;
    let snap = second.snapshot(ids[5]).unwrap();
    assert!(snap.force_start);
    assert_eq!(snap.queue_position, pos);
    assert_ne!(snap.state, TorrentState::Queued);
    assert_ne!(snap.state, TorrentState::Paused);
    second.shutdown();
}

#[tokio::test]
async fn alt_mode_switches_effective_limits() {
    let session = session_with(SessionOptions {
        download_limit: 1000,
        alt_download_limit: 2000,
        ..SessionOptions::default()
    })
    .await;
    assert_eq!(session.transfer_stats().download_limit, 1000);
    session.set_alt_mode(true);
    assert!(session.transfer_stats().alt_mode);
    assert_eq!(session.transfer_stats().download_limit, 2000);
    session.set_alt_limits(3000, 4000);
    assert_eq!(session.transfer_stats().download_limit, 3000);
    assert_eq!(session.transfer_stats().alt_upload_limit, 4000);
    session.set_alt_mode(false);
    assert_eq!(session.transfer_stats().download_limit, 1000);
    session.shutdown();
}
