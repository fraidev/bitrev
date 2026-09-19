//! Localhost swarm throughput bench.
//!
//! Spins N in-process seeders (real `Session` seeders by default, or
//! `SeederPeer` when latency is set) and downloads a generated torrent.
//! Binds only to 127.0.0.1. Never talks to the public network.
//!
//! ```text
//! cargo run --release -p bit_rev --example bench_swarm -- --size-mib 64
//! cargo run --release -p bit_rev --example bench_swarm -- --size-mib 64 --json
//! ```

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bit_rev::session::AddTorrentOptions;
use clap::Parser;
use serde::Serialize;
use testkit::{
    add_download, resource_usage, test_session, unique_temp_dir, wait_for_completion,
    ResourceUsage, SeederConfig, SeederPeer, TorrentFixture,
};

const FIXTURE_SEED: u64 = 0xBE4C_0001;
const BLOCK_SIZE: u64 = 16 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "bench_swarm",
    about = "Download a generated torrent from N localhost seeders and print a table"
)]
struct Args {
    /// Payload size in MiB
    #[arg(long, default_value_t = 64)]
    size_mib: u64,

    /// Number of in-process seeders
    #[arg(long, default_value_t = 4)]
    seeders: usize,

    /// Piece length in bytes
    #[arg(long, default_value_t = 256 * 1024)]
    piece_length: u32,

    /// Artificial seeder read/write latency in milliseconds. Non-zero selects SeederPeer.
    #[arg(long = "latency-ms", visible_alias = "latency", default_value_t = 0)]
    latency_ms: u64,

    /// Force the mock SeederPeer even when latency is 0
    #[arg(long)]
    seeder_peer: bool,

    /// Emit machine-readable JSON on stdout instead of the table
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy)]
enum SeederKind {
    Session,
    SeederPeer,
}

impl SeederKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::SeederPeer => "seeder_peer",
        }
    }
}

#[derive(Serialize)]
struct BenchReport {
    size_mib: u64,
    seeders: usize,
    piece_length: u32,
    latency_ms: u64,
    seeder_kind: &'static str,
    wall_secs: f64,
    mib_per_sec: f64,
    peak_rss_bytes: Option<u64>,
    user_secs: Option<f64>,
    sys_secs: Option<f64>,
    bytes_received: u64,
    payload_bytes: u64,
    overhead_ratio: f64,
    duplicate_bytes: u64,
    duplicate_blocks: Option<u64>,
}

struct SwarmOutcome {
    wall: Duration,
    usage_before: ResourceUsage,
    usage_after: ResourceUsage,
    bytes_received: u64,
    payload_bytes: u64,
    duplicate_bytes: u64,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(err) = run(args).await {
        eprintln!("bench_swarm: {err:#}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    if args.size_mib == 0 {
        anyhow::bail!("--size-mib must be greater than 0");
    }
    if args.seeders == 0 {
        anyhow::bail!("--seeders must be greater than 0");
    }
    if args.piece_length == 0 {
        anyhow::bail!("--piece-length must be greater than 0");
    }

    let kind = if args.seeder_peer || args.latency_ms > 0 {
        SeederKind::SeederPeer
    } else {
        SeederKind::Session
    };

    let payload_bytes = args.size_mib.saturating_mul(1024 * 1024);
    let fixture = Arc::new(
        TorrentFixture::builder()
            .single_file("bench.bin", payload_bytes)
            .piece_length(args.piece_length)
            .seed(FIXTURE_SEED)
            .keep_payload(false)
            .build(),
    );

    let timeout = download_timeout(args.size_mib, args.latency_ms);
    let outcome = match kind {
        SeederKind::Session => run_session_seeders(&fixture, args.seeders, timeout).await?,
        SeederKind::SeederPeer => {
            run_seeder_peers(&fixture, args.seeders, args.latency_ms, timeout).await?
        }
    };

    let wall_secs = outcome.wall.as_secs_f64();
    let mib = payload_bytes as f64 / (1024.0 * 1024.0);
    let mib_per_sec = if wall_secs > 0.0 {
        mib / wall_secs
    } else {
        0.0
    };
    let user_secs = sub_opt_f64(
        outcome.usage_after.user_secs,
        outcome.usage_before.user_secs,
    );
    let sys_secs = sub_opt_f64(outcome.usage_after.sys_secs, outcome.usage_before.sys_secs);
    let overhead_ratio = if outcome.payload_bytes == 0 {
        0.0
    } else {
        outcome.bytes_received as f64 / outcome.payload_bytes as f64
    };
    let duplicate_blocks = if outcome.duplicate_bytes == 0 {
        Some(0)
    } else {
        // TorrentDownloadedState exposes duplicate bytes, not a block count.
        Some(outcome.duplicate_bytes.div_ceil(BLOCK_SIZE))
    };

    let report = BenchReport {
        size_mib: args.size_mib,
        seeders: args.seeders,
        piece_length: args.piece_length,
        latency_ms: args.latency_ms,
        seeder_kind: kind.as_str(),
        wall_secs,
        mib_per_sec,
        peak_rss_bytes: outcome.usage_after.peak_rss_bytes,
        user_secs,
        sys_secs,
        bytes_received: outcome.bytes_received,
        payload_bytes: outcome.payload_bytes,
        overhead_ratio,
        duplicate_bytes: outcome.duplicate_bytes,
        duplicate_blocks,
    };

    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        print_table(&report);
    }
    Ok(())
}

fn download_timeout(size_mib: u64, latency_ms: u64) -> Duration {
    let base = 180u64.saturating_add(size_mib.saturating_mul(2));
    let latency_budget = latency_ms.saturating_mul(size_mib).saturating_div(8);
    Duration::from_secs(base.saturating_add(latency_budget).max(180))
}

fn sub_opt_f64(after: Option<f64>, before: Option<f64>) -> Option<f64> {
    Some((after? - before?).max(0.0))
}

async fn run_session_seeders(
    fixture: &Arc<TorrentFixture>,
    n: usize,
    timeout: Duration,
) -> anyhow::Result<SwarmOutcome> {
    let seed_path = fixture.files[0].disk_path.clone();
    let mut seeders = Vec::with_capacity(n);
    let mut addrs = Vec::with_capacity(n);
    for _ in 0..n {
        let session = test_session(None).await;
        session
            .add_torrent(
                AddTorrentOptions::from(fixture.torrent_meta.clone())
                    .output_dir(&seed_path)
                    .seed(true),
            )
            .await?;
        addrs.push(session.wait_listening().await);
        seeders.push(session);
    }

    let outcome = download_from(fixture, &addrs, timeout).await?;
    for session in seeders {
        session.shutdown();
    }
    Ok(outcome)
}

async fn run_seeder_peers(
    fixture: &Arc<TorrentFixture>,
    n: usize,
    latency_ms: u64,
    timeout: Duration,
) -> anyhow::Result<SwarmOutcome> {
    let latency = Duration::from_millis(latency_ms);
    let mut seeders = Vec::with_capacity(n);
    let mut addrs = Vec::with_capacity(n);
    for i in 0..n {
        let mut peer_id = *b"-SDBNCH-0123456789ab";
        peer_id[19] = i as u8;
        let seeder = SeederPeer::start(
            fixture.clone(),
            SeederConfig::all_pieces().peer_id(peer_id).latency(latency),
        )
        .await;
        addrs.push(seeder.addr);
        seeders.push(seeder);
    }

    let outcome = download_from(fixture, &addrs, timeout).await?;
    drop(seeders);
    Ok(outcome)
}

fn loopback_connect_addr(addr: SocketAddr) -> anyhow::Result<SocketAddr> {
    if addr.ip().is_loopback() {
        return Ok(addr);
    }
    // Session binds 0.0.0.0:<port>. Rewrite to loopback so the bench never
    // dials a routable address.
    if addr.ip().is_unspecified() {
        return Ok(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            addr.port(),
        ));
    }
    anyhow::bail!("refusing non-localhost seeder {addr}");
}

async fn download_from(
    fixture: &TorrentFixture,
    addrs: &[SocketAddr],
    timeout: Duration,
) -> anyhow::Result<SwarmOutcome> {
    let addrs: Vec<SocketAddr> = addrs
        .iter()
        .copied()
        .map(loopback_connect_addr)
        .collect::<anyhow::Result<_>>()?;

    let download_dir = unique_temp_dir();
    let output = fixture.session_output(download_dir.path());
    let leecher = test_session(None).await;
    let added = add_download(&leecher, fixture.torrent_meta.clone(), output.clone()).await;
    let info_hash = fixture.torrent_meta.info_hash;

    let usage_before = resource_usage();
    let start = Instant::now();
    for addr in addrs {
        if !leecher.connect_peer(&info_hash, addr) {
            anyhow::bail!("failed to connect to seeder {addr}");
        }
    }
    wait_for_completion(&added.pr_rx, &added.torrent, &added.already_have, timeout).await;
    let wall = start.elapsed();
    let usage_after = resource_usage();

    let torrent = leecher
        .torrent_session(&info_hash)
        .ok_or_else(|| anyhow::anyhow!("leecher torrent missing after download"))?;
    let duplicate_bytes = torrent.downloaded_state.duplicate_bytes();
    let downloaded = torrent.downloaded_state.downloaded_bytes();
    let bytes_received = downloaded.saturating_add(duplicate_bytes);
    leecher.shutdown();

    fixture.assert_output_matches(&output);

    Ok(SwarmOutcome {
        wall,
        usage_before,
        usage_after,
        bytes_received,
        payload_bytes: fixture.total_length,
        duplicate_bytes,
    })
}

fn print_table(report: &BenchReport) {
    println!("bitrev localhost swarm");
    println!("----------------------");
    println!("size              {} MiB", report.size_mib);
    println!(
        "seeders           {} ({})",
        report.seeders, report.seeder_kind
    );
    println!("piece length      {} B", report.piece_length);
    println!("latency           {} ms", report.latency_ms);
    println!("wall-clock        {:.3} s", report.wall_secs);
    println!("throughput        {:.2} MiB/s", report.mib_per_sec);
    println!("peak RSS          {}", opt_bytes(report.peak_rss_bytes));
    println!("user CPU          {}", opt_secs(report.user_secs));
    println!("sys CPU           {}", opt_secs(report.sys_secs));
    println!("bytes received    {}", report.bytes_received);
    println!("payload           {}", report.payload_bytes);
    println!("overhead          {:.3}x", report.overhead_ratio);
    println!("duplicate bytes   {}", report.duplicate_bytes);
    match report.duplicate_blocks {
        Some(n) => println!("duplicate blocks  {n} (from duplicate bytes / 16 KiB)"),
        None => println!("duplicate blocks  n/a"),
    }
}

fn opt_bytes(v: Option<u64>) -> String {
    match v {
        Some(n) => format!("{n} B"),
        None => "n/a".into(),
    }
}

fn opt_secs(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("{n:.3} s"),
        None => "n/a".into(),
    }
}
