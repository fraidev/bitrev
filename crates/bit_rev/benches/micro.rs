//! Micro benchmarks for hot bit_rev paths.
//!
//! Run with `cargo bench -p bit_rev` (or `cargo bench -- --test` in CI smoke).

use std::fs;
use std::path::{Path, PathBuf};

use bit_rev::file::{self, AnnounceEvent, AnnounceParams, File, Info, TorrentFile, TorrentMeta};
use bit_rev::message::{self, Message};
use bit_rev::resume::{self, ResumeData, ResumeFile, RESUME_VERSION};
use bit_rev::torrent::Torrent;
use bit_rev::utils;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use serde_bytes::ByteBuf;

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

fn load_samples() -> Vec<(String, Vec<u8>)> {
    let dir = samples_dir();
    let mut samples = Vec::new();
    let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("read samples dir {dir:?}: {e}"));
    for entry in entries {
        let entry = entry.expect("sample dirent");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("torrent") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("sample")
            .to_string();
        let bytes = fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        samples.push((name, bytes));
    }
    samples.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(
        !samples.is_empty(),
        "no .torrent files under {}",
        dir.display()
    );
    samples
}

fn bench_metainfo_parse(c: &mut Criterion) {
    let samples = load_samples();
    let mut group = c.benchmark_group("metainfo_parse");
    for (name, bytes) in &samples {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), bytes, |b, bytes| {
            b.iter(|| file::from_bytes(black_box(bytes)).expect("parse sample"));
        });
    }
    group.finish();
}

fn piece_16k() -> (Message, Vec<u8>) {
    let data = vec![0xCDu8; 16 * 1024];
    let msg = message::format_piece(7, 0, data);
    let serialized = message::serialize(Some(msg.clone()));
    (msg, serialized)
}

fn bench_message_serialize(c: &mut Criterion) {
    let (msg, serialized) = piece_16k();
    let mut group = c.benchmark_group("message_serialize");
    group.throughput(Throughput::Bytes(serialized.len() as u64));
    group.bench_function("piece_16kib", |b| {
        b.iter(|| message::serialize(Some(black_box(msg.clone()))));
    });
    group.finish();
}

fn bench_message_parse(c: &mut Criterion) {
    let (_msg, serialized) = piece_16k();
    let length_buf = &serialized[..4];
    let message_buf = &serialized[4..];
    let mut group = c.benchmark_group("message_parse");
    group.throughput(Throughput::Bytes(serialized.len() as u64));
    group.bench_function("piece_16kib", |b| {
        b.iter(|| message::read(black_box(length_buf), black_box(message_buf)).expect("parse"));
    });
    group.finish();
}

fn sha1_of(data: &[u8]) -> [u8; 20] {
    bit_rev::utils::sha1_digest(data)
}

fn bench_sha1_piece(c: &mut Criterion) {
    let data_256k = vec![0xABu8; 256 * 1024];
    let data_1m = vec![0xABu8; 1024 * 1024];
    let hash_256k = sha1_of(&data_256k);
    let hash_1m = sha1_of(&data_1m);

    let mut group = c.benchmark_group("sha1_piece");
    group.throughput(Throughput::Bytes(data_256k.len() as u64));
    group.bench_function("256kib", |b| {
        b.iter(|| utils::check_integrity(black_box(&hash_256k), black_box(&data_256k)));
    });
    group.throughput(Throughput::Bytes(data_1m.len() as u64));
    group.bench_function("1mib", |b| {
        b.iter(|| utils::check_integrity(black_box(&hash_1m), black_box(&data_1m)));
    });
    group.finish();
}

fn many_file_torrent() -> Torrent {
    const N_FILES: usize = 128;
    const FILE_LEN: i64 = 1500;
    const PIECE_LEN: i64 = 16 * 1024;
    let files: Vec<File> = (0..N_FILES)
        .map(|i| File {
            path: vec![format!("f{i:04}.bin")],
            length: FILE_LEN,
            md5sum: None,
        })
        .collect();
    let total: i64 = FILE_LEN * N_FILES as i64;
    let n_pieces = (total as usize).div_ceil(PIECE_LEN as usize);
    let meta = TorrentMeta::new(TorrentFile {
        info: Info {
            name: "many".into(),
            pieces: ByteBuf::from(vec![0u8; n_pieces * 20]),
            piece_length: PIECE_LEN,
            md5sum: None,
            length: None,
            files: Some(files),
            private: None,
            path: None,
            root_hash: None,
        },
        announce: None,
        nodes: None,
        encoding: None,
        httpseeds: None,
        announce_list: None,
        creation_date: None,
        comment: None,
        created_by: None,
    })
    .expect("many-file metainfo");
    Torrent::new(&meta).expect("many-file torrent")
}

fn bench_map_piece_to_files(c: &mut Criterion) {
    let torrent = many_file_torrent();
    let piece_count = (torrent.length as usize)
        .div_ceil(torrent.piece_length as usize)
        .max(1);
    let mut group = c.benchmark_group("map_piece_to_files");
    group.throughput(Throughput::Elements(piece_count as u64));
    group.bench_function("128_files", |b| {
        b.iter(|| {
            let mut n = 0usize;
            for i in 0..piece_count {
                n += utils::map_piece_to_files(black_box(&torrent), black_box(i)).len();
            }
            n
        });
    });
    group.finish();
}

fn sample_announce_inputs() -> (TorrentMeta, [u8; 20], AnnounceParams) {
    let samples = load_samples();
    let meta = file::from_bytes(&samples[0].1).expect("sample meta");
    let peer_id = *b"-BR0100-benchpeerid!";
    let params = AnnounceParams {
        uploaded: 100,
        downloaded: 200,
        left: 300,
        port: 6881,
        event: Some(AnnounceEvent::Started),
        numwant: AnnounceParams::DEFAULT_NUMWANT,
        key: 0xDF45_C574,
        tracker_id: None,
    };
    (meta, peer_id, params)
}

fn bench_build_tracker_url(c: &mut Criterion) {
    let (meta, peer_id, params) = sample_announce_inputs();
    c.bench_function("build_tracker_url", |b| {
        b.iter(|| {
            file::build_tracker_url(
                black_box(&meta),
                black_box(&peer_id),
                black_box("http://tracker.example/announce"),
                black_box(&params),
            )
        });
    });
}

fn sample_resume() -> ResumeData {
    ResumeData {
        version: RESUME_VERSION,
        info_hash: ByteBuf::from(vec![0xab; 20]),
        bitfield: ByteBuf::from(vec![0b1010_0000; 64]),
        output_dir: "/tmp/out.bin".into(),
        files: vec![ResumeFile {
            path: vec!["out.bin".into()],
            length: 16,
            mtime: 1_700_000_000,
        }],
        uploaded: 11,
        downloaded: 16,
        torrent_path: "/tmp/torrents/ab.torrent".into(),
        paused: 1,
        added_at: 1_700_000_000,
        completed_at: 0,
        category: String::new(),
        tags: Vec::new(),
        sequential: 0,
        file_priorities: Vec::new(),
        save_path: "/tmp/out.bin".into(),
        auto_tmm: 0,
        queue_position: 0,
        force_start: 0,
        ratio_limit: 0,
        seeding_time_limit: 0,
    }
}

fn bench_resume_codec(c: &mut Criterion) {
    let data = sample_resume();
    let encoded = resume::encode(&data).expect("encode resume");

    let mut group = c.benchmark_group("resume");
    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function("encode", |b| {
        b.iter(|| resume::encode(black_box(&data)).expect("encode"));
    });
    group.bench_function("decode", |b| {
        b.iter(|| resume::decode(black_box(&encoded)).expect("decode"));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_metainfo_parse,
    bench_message_serialize,
    bench_message_parse,
    bench_sha1_piece,
    bench_map_piece_to_files,
    bench_build_tracker_url,
    bench_resume_codec,
);
criterion_main!(benches);
