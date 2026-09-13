mod common;

use std::sync::Arc;
use std::time::Duration;

use bit_rev::magnet::Magnet;
use bit_rev::resume;
use bit_rev::session::AddTorrentOptions;
use common::{
    add_download, test_session, unique_temp_dir, wait_for_completion, TorrentFixture,
    DEFAULT_PIECE_LENGTH, DOWNLOAD_TIMEOUT,
};

fn hex_hash(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_x_pe_downloads_and_hash_verifies() {
    let fixture = Arc::new(TorrentFixture::single(
        64 * 1024,
        DEFAULT_PIECE_LENGTH,
        0x6A61_0006,
    ));

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
        .expect("add seeder");
    let seeder_addr = seeder.wait_listening().await;

    let magnet = format!(
        "magnet:?xt=urn:btih:{}&dn={}&x.pe={}",
        hex_hash(&fixture.torrent_meta.info_hash),
        fixture.name,
        seeder_addr
    );
    let parsed = Magnet::parse(&magnet).expect("magnet");
    assert_eq!(parsed.info_hash, fixture.torrent_meta.info_hash);
    assert_eq!(parsed.peers, [seeder_addr]);

    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let leecher = test_session(None).await;
    let added = leecher
        .add_torrent(AddTorrentOptions::from_magnet(&parsed).output_dir(output.clone()))
        .await
        .expect("add magnet");
    assert!(added.is_fetching_metadata());
    let torrent = tokio::time::timeout(Duration::from_secs(15), added.metadata())
        .await
        .expect("metadata timeout")
        .expect("metadata");
    assert_eq!(torrent.info_hash, fixture.torrent_meta.info_hash);
    assert_eq!(torrent.piece_hashes, fixture.piece_hashes);

    wait_for_completion(
        &added.pr_rx,
        torrent.as_ref(),
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    fixture.assert_output_matches(&output);
    leecher.shutdown();
    seeder.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_restart_after_metadata_readds_from_cache() {
    let fixture = Arc::new(TorrentFixture::single(
        48 * 1024,
        DEFAULT_PIECE_LENGTH,
        0x6A61_0007,
    ));

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
        .expect("add seeder");
    let seeder_addr = seeder.wait_listening().await;

    let state_dir = unique_temp_dir();
    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let magnet = Magnet::parse(&format!(
        "magnet:?xt=urn:btih:{}&x.pe={}",
        hex_hash(&fixture.torrent_meta.info_hash),
        seeder_addr
    ))
    .unwrap();

    let leecher = test_session(Some(state_dir.path().to_path_buf())).await;
    let added = leecher
        .add_torrent(AddTorrentOptions::from_magnet(&magnet).output_dir(output.clone()))
        .await
        .expect("add magnet");
    let torrent = tokio::time::timeout(Duration::from_secs(15), added.metadata())
        .await
        .expect("metadata timeout")
        .expect("metadata");
    wait_for_completion(
        &added.pr_rx,
        torrent.as_ref(),
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    fixture.assert_output_matches(&output);
    leecher.shutdown_graceful().await;
    drop(leecher);

    let cached = resume::torrent_cache_path(state_dir.path(), &fixture.torrent_meta.info_hash);
    assert!(
        cached.exists(),
        "cached .torrent should exist at {}",
        cached.display()
    );
    let resume_path = resume::resume_path(state_dir.path(), &fixture.torrent_meta.info_hash);
    assert!(
        resume_path.exists(),
        "resume file should exist after metadata"
    );

    let meta = bit_rev::file::from_filename(cached.to_str().unwrap()).expect("load cache");
    assert_eq!(meta.info_hash, fixture.torrent_meta.info_hash);

    let restarted = test_session(Some(state_dir.path().to_path_buf())).await;
    let added = add_download(&restarted, meta, output.clone()).await;
    assert_eq!(
        added.already_have.len(),
        fixture.piece_count(),
        "restart should trust cached torrent + resume bitfield"
    );
    wait_for_completion(
        &added.pr_rx,
        &added.torrent,
        &added.already_have,
        DOWNLOAD_TIMEOUT,
    )
    .await;
    fixture.assert_output_matches(&output);
    restarted.shutdown();
    seeder.shutdown();
}

#[test]
fn hex_and_base32_of_same_hash_parse_identically() {
    let hash = [
        0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
        0x88, 0x99, 0xaa, 0xbb, 0xcc,
    ];
    let hex = hex_hash(&hash);
    let b32 = encode_base32(&hash);
    let from_hex = Magnet::parse(&format!("magnet:?xt=urn:btih:{hex}")).unwrap();
    let from_b32 = Magnet::parse(&format!("magnet:?xt=urn:btih:{b32}")).unwrap();
    assert_eq!(from_hex.info_hash, from_b32.info_hash);
    assert_eq!(from_hex.info_hash, hash);
}

fn encode_base32(bytes: &[u8; 20]) -> String {
    const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = String::new();
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPH[((acc >> bits) & 31) as usize] as char);
            acc &= (1 << bits) - 1;
        }
    }
    if bits > 0 {
        out.push(ALPH[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}
