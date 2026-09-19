mod common;

use std::time::Duration;

use bit_rev::session::{AddTorrentOptions, Session, SessionEvent, SessionOptions, TorrentState};
use tokio::time::timeout;

use common::{
    add_download, test_session, unique_temp_dir, wait_for_completion, SeederConfig, SeederPeer,
    TorrentFixture, DEFAULT_PIECE_LENGTH, DOWNLOAD_TIMEOUT, LISTEN_TIMEOUT,
};

fn unique_peer_id(tag: u8) -> [u8; 20] {
    let mut id = *b"-SDIT01-............";
    id[19] = tag;
    id
}

fn named_fixture(name: &str, seed: u64) -> TorrentFixture {
    TorrentFixture::builder()
        .single_file(name, 64 * 1024)
        .piece_length(DEFAULT_PIECE_LENGTH)
        .seed(seed)
        .build()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_one_leaves_the_other_downloading() {
    let a = std::sync::Arc::new(named_fixture("a.bin", 0xA11));
    let b = std::sync::Arc::new(named_fixture("b.bin", 0xB11));
    let seeder_a = SeederPeer::start(
        a.clone(),
        SeederConfig::all_pieces().peer_id(unique_peer_id(1)),
    )
    .await;
    let seeder_b = SeederPeer::start(
        b.clone(),
        SeederConfig::all_pieces().peer_id(unique_peer_id(2)),
    )
    .await;

    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let added_a = add_download(
        &session,
        a.torrent_meta.clone(),
        a.session_output(download_dir.path()),
    )
    .await;
    let added_b = add_download(
        &session,
        b.torrent_meta.clone(),
        b.session_output(download_dir.path()),
    )
    .await;

    session.pause(added_a.id).expect("pause a");
    assert_eq!(
        session.snapshot(added_a.id).unwrap().state,
        TorrentState::Paused
    );

    assert!(session.connect_peer(&b.torrent_meta.info_hash, seeder_b.addr));
    let _ = session.connect_peer(&a.torrent_meta.info_hash, seeder_a.addr);

    wait_for_completion(
        &added_b.pr_rx,
        &added_b.torrent,
        &added_b.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;

    let snap_a = session.snapshot(added_a.id).unwrap();
    let snap_b = session.snapshot(added_b.id).unwrap();
    assert_eq!(snap_a.state, TorrentState::Paused);
    assert_eq!(snap_a.downloaded, 0);
    assert!(
        snap_b.state == TorrentState::Seeding || snap_b.progress >= 1.0,
        "b should finish, got {:?}",
        snap_b.state
    );
    assert_eq!(snap_b.downloaded, snap_b.size);
    session.shutdown();
}

#[tokio::test]
async fn list_and_snapshot_contents() {
    let fixture = named_fixture("listed.bin", 0xC11);
    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(output.clone())
                .category("movies")
                .tags(vec!["hd".into()])
                .sequential(true),
        )
        .await
        .unwrap();

    let listed = session.list();
    assert_eq!(listed.len(), 1);
    let snap = session.snapshot(added.id).expect("snapshot");
    assert_eq!(snap.id, added.id);
    assert_eq!(snap.id.to_string(), format!("{}", added.id));
    assert_eq!(snap.id.to_string().len(), 40);
    assert_eq!(snap.name, "listed.bin");
    assert_eq!(snap.info_hash, fixture.torrent_meta.info_hash);
    assert_eq!(snap.size, fixture.total_length);
    assert_eq!(snap.save_path, output);
    assert_eq!(snap.category, "movies");
    assert_eq!(snap.tags, vec!["hd".to_string()]);
    assert!(snap.sequential);
    assert_eq!(snap.piece_count, fixture.piece_count() as u32);
    assert!(matches!(
        snap.state,
        TorrentState::Downloading | TorrentState::Seeding
    ));
    session.shutdown();
}

#[tokio::test]
async fn subscribe_sees_added_state_changed_removed() {
    let fixture = named_fixture("events.bin", 0xD11);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let mut rx = session.subscribe();
    let added = add_download(
        &session,
        fixture.torrent_meta.clone(),
        fixture.session_output(download_dir.path()),
    )
    .await;

    session.pause(added.id).expect("pause for state change");
    session.remove_torrent(added.id, false).expect("remove");

    let mut saw_added = false;
    let mut saw_state = false;
    let mut saw_removed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !(saw_added && saw_state && saw_removed) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match timeout(remaining, rx.recv()).await {
            Ok(Ok(SessionEvent::Added { id })) if id == added.id => saw_added = true,
            Ok(Ok(SessionEvent::StateChanged { id, .. })) if id == added.id => saw_state = true,
            Ok(Ok(SessionEvent::Removed { id })) if id == added.id => saw_removed = true,
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert!(saw_added, "missing Added");
    assert!(saw_state, "missing StateChanged");
    assert!(saw_removed, "missing Removed");
    session.shutdown();
}

#[tokio::test]
async fn duplicate_add_returns_same_id() {
    let fixture = named_fixture("dup.bin", 0xE11);
    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let session = test_session(None).await;
    let first = add_download(&session, fixture.torrent_meta.clone(), output.clone()).await;
    let second = add_download(&session, fixture.torrent_meta.clone(), output).await;
    assert_eq!(first.id, second.id);
    assert_eq!(session.list().len(), 1);
    session.shutdown();
}

#[tokio::test]
async fn transfer_stats_sums_both_torrents() {
    let a = named_fixture("sum-a.bin", 0xF11);
    let b = named_fixture("sum-b.bin", 0xF12);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    session
        .add_torrent(
            AddTorrentOptions::from(a.torrent_meta.clone())
                .output_dir(a.session_output(download_dir.path()))
                .seed(true),
        )
        .await
        .unwrap();
    session
        .add_torrent(
            AddTorrentOptions::from(b.torrent_meta.clone())
                .output_dir(b.session_output(download_dir.path()))
                .seed(true),
        )
        .await
        .unwrap();

    let stats = session.transfer_stats();
    assert_eq!(stats.torrents, 2);
    assert_eq!(stats.downloaded, a.total_length + b.total_length);
    session.shutdown();
}

#[tokio::test]
async fn remove_torrent_deletes_files_and_resume() {
    let fixture = named_fixture("gone.bin", 0x111);
    let root = unique_temp_dir();
    let state_dir = root.path().join("state");
    let output = fixture.session_output(root.path());
    let session = test_session(Some(state_dir.clone())).await;
    let added = add_download(&session, fixture.torrent_meta.clone(), output.clone()).await;
    session.flush_resume().await;

    let resume_path = bit_rev::resume::resume_path(&state_dir, &fixture.torrent_meta.info_hash);
    let cache_path =
        bit_rev::resume::torrent_cache_path(&state_dir, &fixture.torrent_meta.info_hash);
    assert!(resume_path.exists(), "resume should exist after flush");
    assert!(cache_path.exists(), "cached torrent should exist");

    session
        .remove_torrent(added.id, true)
        .expect("remove with delete_files");

    assert!(!resume_path.exists());
    assert!(!cache_path.exists());
    assert!(!output.exists(), "payload file should be deleted");
    assert!(session.snapshot(added.id).is_none());
    session.shutdown();
}

#[tokio::test]
async fn session_open_reloads_paused_and_unpaused() {
    let paused = named_fixture("open-paused.bin", 0x211);
    let active = named_fixture("open-active.bin", 0x212);
    let root = unique_temp_dir();
    let state_dir = root.path().join("state");
    let first = test_session(Some(state_dir.clone())).await;
    let paused_add = first
        .add_torrent(
            AddTorrentOptions::from(paused.torrent_meta.clone())
                .output_dir(paused.session_output(root.path()))
                .paused(true),
        )
        .await
        .unwrap();
    let active_add = first
        .add_torrent(
            AddTorrentOptions::from(active.torrent_meta.clone())
                .output_dir(active.session_output(root.path())),
        )
        .await
        .unwrap();
    first.flush_resume().await;
    let paused_id = paused_add.id;
    let active_id = active_add.id;
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

    assert_eq!(second.list().len(), 2);
    assert_eq!(
        second.snapshot(paused_id).unwrap().state,
        TorrentState::Paused
    );
    assert_ne!(
        second.snapshot(active_id).unwrap().state,
        TorrentState::Paused
    );
    second.shutdown();
}
