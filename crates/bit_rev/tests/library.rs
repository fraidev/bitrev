mod common;

use std::time::Duration;

use bit_rev::session::{AddTorrentOptions, Category, Session, SessionOptions, TorrentState};
use tokio::time::timeout;

use common::{test_session, unique_temp_dir, TorrentFixture, DEFAULT_PIECE_LENGTH, LISTEN_TIMEOUT};

fn named_fixture(name: &str, seed: u64) -> TorrentFixture {
    TorrentFixture::builder()
        .single_file(name, 64 * 1024)
        .piece_length(DEFAULT_PIECE_LENGTH)
        .seed(seed)
        .build()
}

async fn wait_list_len(session: &Session, n: usize, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if session.list().len() == n {
            return;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "session list len {}, want {n}: {:?}",
                session.list().len(),
                session.list()
            );
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(50))).await;
    }
}

#[tokio::test]
async fn sonarr_add_stores_category_and_save_path() {
    let fixture = named_fixture("show.bin", 0x3701);
    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .save_path(output.clone())
                .category("tv-sonarr")
                .paused(true),
        )
        .await
        .unwrap();

    let snap = session.snapshot(added.id).expect("snapshot");
    assert_eq!(snap.category, "tv-sonarr");
    assert_eq!(snap.save_path, output);
    assert_eq!(snap.state, TorrentState::Paused);
    assert!(
        session.categories().iter().any(|c| c.name == "tv-sonarr"),
        "adding with an unknown category should create it: {:?}",
        session.categories()
    );
    session.shutdown();
}

#[tokio::test]
async fn category_created_on_add_is_visible() {
    let fixture = named_fixture("cat.bin", 0x3702);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(fixture.session_output(download_dir.path()))
                .category("movies"),
        )
        .await
        .unwrap();
    assert_eq!(session.snapshot(added.id).unwrap().category, "movies");
    assert_eq!(session.categories(), vec![Category::new("movies", None)]);
    session.shutdown();
}

#[tokio::test]
async fn categories_survive_session_open() {
    let fixture = named_fixture("keep.bin", 0x3703);
    let root = unique_temp_dir();
    let state_dir = root.path().join("state");
    let first = test_session(Some(state_dir.clone())).await;
    first
        .create_category("tv-sonarr", Some(root.path().join("tv")))
        .expect("create category");
    first
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(fixture.session_output(root.path()))
                .category("tv-sonarr")
                .paused(true),
        )
        .await
        .unwrap();
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

    let cats = second.categories();
    let cat = cats
        .iter()
        .find(|c| c.name == "tv-sonarr")
        .expect("category");
    assert_eq!(
        cat.save_path.as_deref(),
        Some(root.path().join("tv").as_path())
    );
    let snap = second
        .snapshot(bit_rev::session::TorrentId::new(
            fixture.torrent_meta.info_hash,
        ))
        .expect("reloaded torrent");
    assert_eq!(snap.category, "tv-sonarr");
    second.shutdown();
}

#[tokio::test]
async fn tags_round_trip_through_resume() {
    let fixture = named_fixture("tags.bin", 0x3704);
    let root = unique_temp_dir();
    let state_dir = root.path().join("state");
    let first = test_session(Some(state_dir.clone())).await;
    let added = first
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(fixture.session_output(root.path()))
                .tags(vec!["hd".into(), "hdr".into()])
                .paused(true),
        )
        .await
        .unwrap();
    first
        .add_tags(added.id, vec!["4k".into()])
        .expect("add tags");
    first
        .remove_tags(added.id, vec!["hdr".into()])
        .expect("remove tags");
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
    let snap = second.snapshot(added.id).expect("reloaded");
    assert_eq!(snap.tags, vec!["4k".to_string(), "hd".to_string()]);
    second.shutdown();
}

#[tokio::test]
async fn set_save_path_moves_completed_files() {
    let fixture = named_fixture("move.bin", 0x3705);
    let root = unique_temp_dir();
    let output = fixture.session_output(root.path());
    std::fs::copy(&fixture.files[0].disk_path, &output).unwrap();
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(output.clone())
                .seed(true)
                .skip_checking(true),
        )
        .await
        .unwrap();

    let dest_dir = root.path().join("relocated");
    let dest = dest_dir.join("move.bin");
    session
        .set_save_path(added.id, dest.clone(), true)
        .await
        .expect("relocate");

    let snap = session.snapshot(added.id).unwrap();
    assert_eq!(snap.save_path, dest);
    assert_ne!(snap.state, TorrentState::Moving);
    assert!(!matches!(snap.state, TorrentState::Error(_)));
    assert!(!output.exists(), "old path should be gone");
    fixture.assert_output_matches(&dest);
    session.shutdown();
}

#[tokio::test]
async fn set_save_path_failure_leaves_files_and_sets_error() {
    let fixture = named_fixture("stuck.bin", 0x3706);
    let root = unique_temp_dir();
    let output = fixture.session_output(root.path());
    std::fs::copy(&fixture.files[0].disk_path, &output).unwrap();
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(output.clone())
                .seed(true)
                .skip_checking(true),
        )
        .await
        .unwrap();

    let blocker = root.path().join("not-a-dir");
    std::fs::write(&blocker, b"file").unwrap();
    let dest = blocker.join("stuck.bin");
    let err = session
        .set_save_path(added.id, dest, true)
        .await
        .expect_err("move should fail");
    assert!(
        matches!(err, bit_rev::session::ControlError::MoveFailed { .. }),
        "got {err:?}"
    );

    let snap = session.snapshot(added.id).unwrap();
    assert!(matches!(snap.state, TorrentState::Error(_)));
    assert_eq!(snap.save_path, output);
    fixture.assert_output_matches(&output);
    session.shutdown();
}

#[tokio::test]
async fn watch_dir_adds_torrent_and_ignores_duplicate() {
    let fixture = named_fixture("watched.bin", 0x3707);
    let root = unique_temp_dir();
    let watch = root.path().join("watch");
    let state_dir = root.path().join("state");
    let download_dir = root.path().join("dl");
    std::fs::create_dir_all(&watch).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();

    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir: Some(state_dir.clone()),
        download_dir: download_dir.clone(),
        watch_dir: watch.clone(),
        watch_poll: Duration::from_millis(50),
        encryption: bit_rev::mse::EncryptionPolicy::Disabled,
        ..SessionOptions::default()
    });
    let _ = timeout(LISTEN_TIMEOUT, session.wait_listening()).await;

    let torrent_path = watch.join("watched.torrent");
    std::fs::write(&torrent_path, &fixture.torrent_bytes).unwrap();
    wait_list_len(&session, 1, Duration::from_secs(5)).await;

    let snap = session
        .snapshot(bit_rev::session::TorrentId::new(
            fixture.torrent_meta.info_hash,
        ))
        .expect("watched torrent");
    assert_eq!(snap.name, "watched.bin");
    assert!(!matches!(snap.state, TorrentState::Error(_)));

    let dup = watch.join("dup.torrent");
    std::fs::write(&dup, &fixture.torrent_bytes).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(session.list().len(), 1);
    assert!(!matches!(
        session.snapshot(snap.id).unwrap().state,
        TorrentState::Error(_)
    ));

    let processed = bit_rev::library::watch_processed_path(&state_dir);
    assert!(
        processed.join("watched.torrent").exists()
            || std::fs::read_dir(&processed)
                .map(|entries| entries.count() > 0)
                .unwrap_or(false),
        "watched file should be moved to watch-processed"
    );
    session.shutdown();
}

#[tokio::test]
async fn remove_category_clears_torrent_field() {
    let fixture = named_fixture("uncat.bin", 0x3708);
    let download_dir = unique_temp_dir();
    let session = test_session(None).await;
    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .output_dir(fixture.session_output(download_dir.path()))
                .category("gone"),
        )
        .await
        .unwrap();
    session.remove_category("gone").expect("remove");
    assert!(session.categories().is_empty());
    assert_eq!(session.snapshot(added.id).unwrap().category, "");
    session.shutdown();
}

#[tokio::test]
async fn category_save_path_used_when_add_omits_save_path() {
    let fixture = named_fixture("from-cat.bin", 0x3709);
    let root = unique_temp_dir();
    let cat_dir = root.path().join("tv");
    std::fs::create_dir_all(&cat_dir).unwrap();
    let session = test_session(None).await;
    session
        .create_category("tv", Some(cat_dir.clone()))
        .unwrap();
    let added = session
        .add_torrent(AddTorrentOptions::from(fixture.torrent_meta.clone()).category("tv"))
        .await
        .unwrap();
    let snap = session.snapshot(added.id).unwrap();
    assert_eq!(snap.save_path, cat_dir.join("from-cat.bin"));
    session.shutdown();
}

#[tokio::test]
async fn completed_dir_moves_when_auto_tmm_is_on() {
    let fixture = named_fixture("done.bin", 0x370A);
    let root = unique_temp_dir();
    let download_dir = root.path().join("dl");
    let completed_dir = root.path().join("completed");
    std::fs::create_dir_all(&download_dir).unwrap();
    let output = download_dir.join("done.bin");
    std::fs::copy(&fixture.files[0].disk_path, &output).unwrap();

    let session = Session::with_options(SessionOptions {
        listen_port: 0,
        state_dir: None,
        download_dir: download_dir.clone(),
        completed_dir: completed_dir.clone(),
        encryption: bit_rev::mse::EncryptionPolicy::Disabled,
        ..SessionOptions::default()
    });
    let _ = timeout(LISTEN_TIMEOUT, session.wait_listening()).await;

    let added = session
        .add_torrent(
            AddTorrentOptions::from(fixture.torrent_meta.clone())
                .category("tv-sonarr")
                .auto_tmm(true)
                .seed(true)
                .skip_checking(true),
        )
        .await
        .unwrap();

    let dest = completed_dir.join("tv-sonarr").join("done.bin");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snap = session.snapshot(added.id).unwrap();
        if snap.save_path == dest {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "did not move to completed_dir, save_path={:?}",
                snap.save_path
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    fixture.assert_output_matches(&dest);
    assert!(!output.exists());
    session.shutdown();
}
