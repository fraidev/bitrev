//! Bitrev-side helper for `interop/run.sh`.
//!
//! Generates fixture torrents via `testkit` and drives a `Session` for
//! download/seed. Docker orchestration stays in the shell runner so
//! `cargo test` never needs a daemon.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};
use bit_rev::dht::DhtOptions;
use bit_rev::mse::EncryptionPolicy;
use bit_rev::session::{AddTorrentOptions, PieceResult, Session, SessionOptions};
use bit_rev::torrent::Torrent;
use bit_rev::utp::UtpOptions;
use clap::{Parser, Subcommand, ValueEnum};
use testkit::{hex_encode, sha1_file, FileSpec, TorrentFixture};

#[derive(Debug, Parser)]
#[command(
    name = "bitrev-interop",
    about = "Fixture generator and bitrev driver for the interop harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write a generated torrent and payload into a directory.
    GenFixture {
        /// Output directory for torrent, payload, and hashes
        #[arg(long)]
        dir: PathBuf,
        /// HTTP or UDP announce URL
        #[arg(long)]
        announce: String,
        /// Torrent name and default single-file name
        #[arg(long, default_value = "payload.bin")]
        name: String,
        /// Single-file length in bytes (ignored when --file is set)
        #[arg(long, default_value_t = 256 * 1024)]
        length: u64,
        /// Piece length in bytes
        #[arg(long, default_value_t = 32 * 1024)]
        piece_length: u32,
        /// RNG seed for payload bytes
        #[arg(long, default_value_t = 0x17E0_0001)]
        seed: u64,
        /// Set info.private = 1
        #[arg(long)]
        private: bool,
        /// Repeatable `name:length` entries for a multi-file torrent
        #[arg(long = "file", value_name = "NAME:BYTES")]
        files: Vec<String>,
    },
    /// Download a torrent or magnet and exit when the payload is complete.
    Download {
        #[command(flatten)]
        session: SessionArgs,
        /// Torrent file path or magnet URI
        #[arg(long)]
        torrent: String,
        /// Single-file path or multi-file directory
        #[arg(long)]
        output: PathBuf,
        /// Seconds to wait for every piece
        #[arg(long, default_value_t = 90)]
        timeout: u64,
        /// Expected SHA-1 hex of the concatenated payload (optional)
        #[arg(long)]
        expect_sha1: Option<String>,
    },
    /// Seed an existing payload until SIGINT or SIGTERM.
    Seed {
        #[command(flatten)]
        session: SessionArgs,
        /// Torrent file path
        #[arg(long)]
        torrent: String,
        /// Single-file path or multi-file directory that already holds the data
        #[arg(long)]
        output: PathBuf,
    },
    /// Print the SHA-1 hex of a file.
    Sha1 { path: PathBuf },
}

#[derive(Debug, Clone, clap::Args)]
struct SessionArgs {
    /// TCP listen port (0 = ephemeral)
    #[arg(long, default_value_t = 6881)]
    port: u16,
    /// Resume / DHT state directory
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = EncryptionArg::PreferPlaintext)]
    encryption: EncryptionArg,
    /// Enable Mainline DHT
    #[arg(long, default_value_t = false)]
    dht: bool,
    /// DHT UDP port (0 = same as --port)
    #[arg(long, default_value_t = 0)]
    dht_port: u16,
    /// Extra bootstrap nodes `host:port` (magnet-via-dht)
    #[arg(long = "dht-bootstrap")]
    dht_bootstrap: Vec<String>,
    /// Enable uTP
    #[arg(long, default_value_t = false)]
    utp: bool,
    /// Extra `host:port` peers to dial on a retry loop (bypasses tracker interval)
    #[arg(long = "peer")]
    peers: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EncryptionArg {
    Disabled,
    PreferPlaintext,
    PreferEncrypted,
    RequireEncrypted,
}

impl From<EncryptionArg> for EncryptionPolicy {
    fn from(value: EncryptionArg) -> Self {
        match value {
            EncryptionArg::Disabled => Self::Disabled,
            EncryptionArg::PreferPlaintext => Self::PreferPlaintext,
            EncryptionArg::PreferEncrypted => Self::PreferEncrypted,
            EncryptionArg::RequireEncrypted => Self::RequireEncrypted,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    if let Err(err) = run(cli).await {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::GenFixture {
            dir,
            announce,
            name,
            length,
            piece_length,
            seed,
            private,
            files,
        } => gen_fixture(
            dir,
            announce,
            name,
            length,
            piece_length,
            seed,
            private,
            files,
        ),
        Command::Download {
            session,
            torrent,
            output,
            timeout,
            expect_sha1,
        } => download(session, torrent, output, timeout, expect_sha1).await,
        Command::Seed {
            session,
            torrent,
            output,
        } => seed(session, torrent, output).await,
        Command::Sha1 { path } => {
            println!("{}", hex_encode(&sha1_file(&path)));
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn gen_fixture(
    dir: PathBuf,
    announce: String,
    name: String,
    length: u64,
    piece_length: u32,
    seed: u64,
    private: bool,
    files: Vec<String>,
) -> anyhow::Result<()> {
    let mut builder = TorrentFixture::builder()
        .seed(seed)
        .piece_length(piece_length)
        .announce(&announce)
        .announce_list(vec![vec![announce]])
        .private(private)
        .keep_payload(true);

    if files.is_empty() {
        builder = builder.single_file(&name, length);
    } else {
        builder = builder.name(&name).files(parse_files(&files)?);
    }

    let fixture = builder.build();
    let persisted = fixture.persist_to(&dir);
    println!("torrent {}", persisted.torrent_path.display());
    println!("data {}", persisted.data_dir.display());
    println!("sha1 {}", persisted.sha1_hex);
    println!("info-hash {}", persisted.info_hash_hex);
    Ok(())
}

fn parse_files(entries: &[String]) -> anyhow::Result<Vec<FileSpec>> {
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        let (name, length) = entry
            .rsplit_once(':')
            .with_context(|| format!("expected NAME:BYTES, got {entry}"))?;
        let length: u64 = length
            .parse()
            .with_context(|| format!("invalid length in {entry}"))?;
        let path = name
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if path.is_empty() {
            bail!("empty file name in {entry}");
        }
        files.push(FileSpec { path, length });
    }
    Ok(files)
}

async fn download(
    args: SessionArgs,
    torrent: String,
    output: PathBuf,
    timeout_secs: u64,
    expect_sha1: Option<String>,
) -> anyhow::Result<()> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let session = start_session(&args).await?;
    let opts = AddTorrentOptions::try_from(torrent.as_str())
        .with_context(|| format!("failed to open {torrent}"))?
        .output_dir(&output);
    let result = session
        .add_torrent(opts)
        .await
        .with_context(|| format!("failed to add {torrent}"))?;

    let timeout = Duration::from_secs(timeout_secs);
    let torrent_meta = if result.is_fetching_metadata() {
        result.metadata().await.context("metadata fetch failed")?
    } else {
        std::sync::Arc::new(result.torrent.clone())
    };
    let info_hash = result.info_hash();
    tokio::select! {
        res = wait_complete(
            &result.pr_rx,
            torrent_meta.as_ref(),
            &result.already_have,
            timeout,
        ) => {
            res?;
        }
        _ = dial_peers(&session, info_hash, &args.peers) => {}
    }

    if let Some(expected) = expect_sha1 {
        let expected = expected.trim().to_ascii_lowercase();
        let got = if output.is_file() {
            hex_encode(&sha1_file(&output))
        } else {
            concat_sha1(&output, torrent_meta.as_ref())?
        };
        if got != expected {
            bail!("payload sha1 mismatch: got {got} want {expected}");
        }
        println!("sha1 {got}");
    }

    session.shutdown_graceful().await;
    Ok(())
}

async fn seed(args: SessionArgs, torrent: String, output: PathBuf) -> anyhow::Result<()> {
    if !output.exists() {
        bail!("seed payload missing at {}", output.display());
    }
    let session = start_session(&args).await?;
    let opts = AddTorrentOptions::try_from(torrent.as_str())
        .with_context(|| format!("failed to open {torrent}"))?
        .output_dir(&output)
        .seed(true);
    let result = session
        .add_torrent(opts)
        .await
        .with_context(|| format!("failed to seed {torrent}"))?;
    eprintln!(
        "Seeding {} on port {}.",
        output.display(),
        session.listen_port()
    );
    tokio::select! {
        _ = shutdown_signal() => {}
        _ = dial_peers(&session, result.info_hash(), &args.peers) => {}
    }
    session.shutdown_graceful().await;
    Ok(())
}

async fn dial_peers(session: &Session, info_hash: [u8; 20], peers: &[String]) {
    if peers.is_empty() {
        std::future::pending::<()>().await;
        return;
    }
    loop {
        for peer in peers {
            match resolve_peer(peer).await {
                Ok(addr) => {
                    if session.connect_peer(&info_hash, addr) {
                        tracing::info!(%addr, "dialed peer");
                    }
                }
                Err(err) => tracing::debug!(peer, error = %err, "resolve peer"),
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn resolve_peer(spec: &str) -> anyhow::Result<std::net::SocketAddr> {
    let mut addrs = tokio::net::lookup_host(spec)
        .await
        .with_context(|| format!("lookup {spec}"))?;
    addrs
        .next()
        .with_context(|| format!("no addresses for {spec}"))
}

async fn start_session(args: &SessionArgs) -> anyhow::Result<Session> {
    let bootstrap = if args.dht_bootstrap.is_empty() && args.dht {
        DhtOptions::default_bootstrap()
    } else {
        args.dht_bootstrap.clone()
    };
    let session = Session::with_options(SessionOptions {
        listen_port: args.port,
        state_dir: args.state_dir.clone(),
        encryption: args.encryption.into(),
        dht: DhtOptions {
            enabled: args.dht,
            port: if args.dht_port == 0 {
                args.port
            } else {
                args.dht_port
            },
            bootstrap_nodes: bootstrap,
        },
        utp: UtpOptions {
            enabled: args.utp,
            port: 0,
        },
        ..SessionOptions::default()
    });
    let addr = tokio::time::timeout(Duration::from_secs(5), session.wait_listening())
        .await
        .context("timed out waiting for listen port")?;
    tracing::info!(%addr, "listening");
    Ok(session)
}

async fn wait_complete(
    pr_rx: &flume::Receiver<PieceResult>,
    torrent: &Torrent,
    already_have: &[PieceResult],
    timeout: Duration,
) -> anyhow::Result<()> {
    let total = torrent.piece_hashes.len();
    if total == 0 {
        bail!("torrent has no pieces");
    }
    let mut seen = vec![false; total];
    for pr in already_have {
        if let Some(slot) = seen.get_mut(pr.index as usize) {
            *slot = true;
        }
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while seen.iter().any(|have| !have) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let have = seen.iter().filter(|h| **h).count();
            bail!("timeout waiting for pieces ({have}/{total})");
        }
        let pr = tokio::time::timeout(remaining, pr_rx.recv_async())
            .await
            .context("timeout waiting for pieces")?
            .context("piece channel closed")?;
        if let Some(slot) = seen.get_mut(pr.index as usize) {
            *slot = true;
        }
    }
    Ok(())
}

fn concat_sha1(root: &std::path::Path, torrent: &Torrent) -> anyhow::Result<String> {
    use std::io::Read;
    let mut hasher = sha1_smol::Sha1::new();
    let mut buf = vec![0u8; 64 * 1024];
    for file in &torrent.files {
        let mut path = root.to_path_buf();
        for component in &file.path {
            path.push(component);
        }
        let mut f =
            std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }
    Ok(hex_encode(&hasher.digest().bytes()))
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

#[cfg(test)]
mod tests {
    use super::parse_files;

    #[test]
    fn parse_files_splits_name_and_length() {
        let files = parse_files(&["a.bin:64".into(), "sub/b.bin:128".into()]).unwrap();
        assert_eq!(files[0].path, ["a.bin"]);
        assert_eq!(files[0].length, 64);
        assert_eq!(files[1].path, ["sub", "b.bin"]);
        assert_eq!(files[1].length, 128);
    }
}
