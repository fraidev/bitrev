mod common;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bit_rev::session::{
    AddTorrentOptions, FilePriority, Session, SessionOptions, TorrentId, TorrentState,
};
use tokio::time::timeout;

use common::{
    sha1_file, test_session, unique_temp_dir, FileSpec, SeederConfig, SeederPeer, TorrentFixture,
    DEFAULT_PIECE_LENGTH, DOWNLOAD_TIMEOUT, LISTEN_TIMEOUT,
};

fn unique_peer_id(tag: u8) -> [u8; 20] {
    let mut id = *b"-SDIT36-............";
    id[19] = tag;
    id
}

fn three_file_fixture(pieces_per_file: u32, seed: u64) -> TorrentFixture {
    let piece = u64::from(DEFAULT_PIECE_LENGTH);
    let file_len = piece * u64::from(pieces_per_file);
    TorrentFixture::builder()
        .name("prio-bundle")
        .seed(seed)
        .piece_length(DEFAULT_PIECE_LENGTH)
        .files(vec![
            FileSpec::new(["a.bin"], file_len),
            FileSpec::new(["skip.bin"], file_len),
            FileSpec::new(["c.bin"], file_len),
        ])
        .build()
}

fn first_seen_order(indexes: &[u32]) -> Vec<u32> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for &index in indexes {
        if seen.insert(index) {
            out.push(index);
        }
    }
    out
}

async fn wait_wanted_complete(session: &Session, id: TorrentId, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(snap) = session.snapshot(id) {
            if snap.left == 0 && snap.progress >= 1.0 {
                return;
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let snap = session.snapshot(id);
            panic!("wanted pieces did not complete in time: {snap:?}");
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(50))).await;
    }
}

fn assert_wanted_file_matches(fixture: &TorrentFixture, output: &Path, file_index: usize) {
    let file = &fixture.files[file_index];
    let mut got = output.to_path_buf();
    for component in &file.path {
        got.push(component);
    }
    assert!(got.is_file(), "missing output file {got:?}");
    assert_eq!(
        sha1_file(&got),
        sha1_file(&file.disk_path),
        "hash mismatch for {got:?}"
    );
}

#[tokio::test]
async fn skipping_every_file_is_complete() {
    let fixture = three_file_fixture(2, 0x3601);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(fixture.session_output(download_dir.path()))
                .file_priorities(vec![FilePriority::Skip; 3]),
        )
        .await
        .unwrap();

    let snap = session.snapshot(added.id).unwrap();
    assert_eq!(snap.state, TorrentState::Seeding);
    assert_eq!(snap.progress, 1.0);
    assert_eq!(snap.left, 0);
    let files = session.files(added.id).unwrap();
    assert_eq!(files.len(), 3);
    assert!(files.iter().all(|f| f.priority == FilePriority::Skip));
    session.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skip_middle_file_never_requests_those_pieces() {
    let fixture = Arc::new(three_file_fixture(2, 0x3602));
    assert_eq!(fixture.piece_count(), 6);
    let seeder = SeederPeer::start(
        fixture.clone(),
        SeederConfig::all_pieces().peer_id(unique_peer_id(1)),
    )
    .await;

    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(output.clone())
                .file_priorities(vec![
                    FilePriority::Normal,
                    FilePriority::Skip,
                    FilePriority::Normal,
                ]),
        )
        .await
        .unwrap();

    assert!(session.connect_peer(&fixture.torrent_meta.info_hash, seeder.addr));
    wait_wanted_complete(&session, added.id, DOWNLOAD_TIMEOUT).await;

    let requested: HashSet<u32> = seeder.requested_pieces().into_iter().collect();
    assert!(
        !requested.contains(&2) && !requested.contains(&3),
        "skipped pieces were requested: {requested:?}"
    );
    assert!(requested.contains(&0) || requested.contains(&1));
    assert!(requested.contains(&4) || requested.contains(&5));

    let snap = session.snapshot(added.id).unwrap();
    assert_eq!(snap.left, 0);
    assert!(snap.progress >= 1.0);
    assert_eq!(snap.state, TorrentState::Seeding);

    assert_wanted_file_matches(&fixture, &output, 0);
    assert_wanted_file_matches(&fixture, &output, 2);
    session.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_requests_increase_apart_from_first_last() {
    let fixture = Arc::new(three_file_fixture(4, 0x3603));
    assert_eq!(fixture.piece_count(), 12);
    let seeder = SeederPeer::start(
        fixture.clone(),
        SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
    )
    .await;

    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(output.clone())
                .sequential(true),
        )
        .await
        .unwrap();

    assert!(session.connect_peer(&fixture.torrent_meta.info_hash, seeder.addr));
    wait_wanted_complete(&session, added.id, DOWNLOAD_TIMEOUT).await;

    let order = first_seen_order(&seeder.requested_pieces());
    let first_last: HashSet<u32> = [0, 3, 4, 7, 8, 11].into_iter().collect();
    for index in &first_last {
        assert!(
            order.contains(index),
            "missing first/last piece {index} in {order:?}"
        );
    }
    let rest: Vec<u32> = order
        .iter()
        .copied()
        .filter(|index| !first_last.contains(index))
        .collect();
    assert!(
        rest.windows(2).all(|w| w[0] < w[1]),
        "non-boundary pieces should increase: {rest:?} (full {order:?})"
    );

    fixture.assert_output_matches(&output);
    session.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn priority_change_cancels_and_requeues() {
    let piece_len = 64 * 1024;
    let fixture = Arc::new(
        TorrentFixture::builder()
            .name("prio-change")
            .seed(0x3604)
            .piece_length(piece_len)
            .files(vec![
                FileSpec::new(["a.bin"], u64::from(piece_len)),
                FileSpec::new(["b.bin"], u64::from(piece_len)),
                FileSpec::new(["c.bin"], u64::from(piece_len)),
            ])
            .build(),
    );
    let seeder = SeederPeer::start(
        fixture.clone(),
        SeederConfig::all_pieces()
            .peer_id(unique_peer_id(3))
            .latency(Duration::from_millis(80)),
    )
    .await;

    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone()).output_dir(output.clone()),
        )
        .await
        .unwrap();
    assert!(session.connect_peer(&fixture.torrent_meta.info_hash, seeder.addr));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if seeder.requested_pieces().contains(&1) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "seeder never saw piece 1, requests {:?}",
                seeder.requested_pieces()
            );
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    session
        .set_file_priority(added.id, 1, FilePriority::Skip)
        .unwrap();
    assert_eq!(
        session.files(added.id).unwrap()[1].priority,
        FilePriority::Skip
    );
    let cancel_deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while seeder.cancels_received() == 0 {
        if tokio::time::Instant::now() >= cancel_deadline {
            panic!(
                "expected cancel after skipping the in-flight file; requests {:?}",
                seeder.requested_pieces()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_wanted_complete(&session, added.id, DOWNLOAD_TIMEOUT).await;
    assert_wanted_file_matches(&fixture, &output, 0);
    assert_wanted_file_matches(&fixture, &output, 2);

    session
        .set_file_priority(added.id, 1, FilePriority::Normal)
        .unwrap();
    wait_wanted_complete(&session, added.id, DOWNLOAD_TIMEOUT).await;
    fixture.assert_output_matches(&output);
    session.shutdown();
}

#[tokio::test]
async fn priorities_and_sequential_survive_resume() {
    let fixture = three_file_fixture(2, 0x3605);
    let root = unique_temp_dir();
    let state_dir = root.path().join("state");
    let output = fixture.session_output(root.path());
    let first = test_session(Some(state_dir.clone())).await;
    let added = first
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(output)
                .sequential(true)
                .file_priorities(vec![
                    FilePriority::High,
                    FilePriority::Skip,
                    FilePriority::Low,
                ]),
        )
        .await
        .unwrap();
    let id = added.id;
    first.flush_resume().await;
    drop(first);

    let second = Session::open(SessionOptions {
        listen_port: 0,
        state_dir: Some(state_dir),
        encryption: bit_rev::mse::EncryptionPolicy::Disabled,
        ..SessionOptions::default()
    })
    .await
    .expect("open");
    let _ = timeout(LISTEN_TIMEOUT, second.wait_listening()).await;

    let snap = second.snapshot(id).expect("reloaded snapshot");
    assert!(snap.sequential);
    assert_eq!(
        second.file_priorities(id).unwrap(),
        vec![FilePriority::High, FilePriority::Skip, FilePriority::Low]
    );
    let files = second.files(id).unwrap();
    assert_eq!(files[1].priority, FilePriority::Skip);
    assert_eq!(files[0].priority, FilePriority::High);
    assert_eq!(files[2].priority, FilePriority::Low);
    assert!(snap.left > 0);
    assert!(snap.progress < 1.0);
    assert_eq!(snap.state, TorrentState::Downloading);
    second.shutdown();
}

#[tokio::test]
async fn set_file_priority_rejects_unknown_index() {
    let fixture = three_file_fixture(1, 0x3606);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(fixture.session_output(download_dir.path())),
        )
        .await
        .unwrap();
    let err = session
        .set_file_priority(added.id, 9, FilePriority::Skip)
        .unwrap_err();
    assert!(matches!(
        err,
        bit_rev::session::ControlError::NoSuchFile { index: 9, .. }
    ));
    session.shutdown();
}
