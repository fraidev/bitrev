use std::collections::HashSet;
use std::fmt::Write;
use std::path::Path;

use anyhow::Context;
use bit_rev::config::Config;
use bit_rev::session::{AddTorrentOptions, AddTorrentResult, Session};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

use crate::args::Cli;

pub async fn run(cli: Cli, config: Config) -> anyhow::Result<()> {
    let mut pending = Vec::with_capacity(cli.inputs.len());
    for input in &cli.inputs {
        pending.push((
            input.clone(),
            add_options(input, &config.download_dir, cli.verify)?,
        ));
    }

    let session = Session::with_options(config.session_options());
    let mp = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
    let style = progress_style();

    let mut join = tokio::task::JoinSet::new();
    for (input, opts) in pending {
        let result = session
            .add_torrent(opts)
            .await
            .with_context(|| format!("failed to add {input}"))?;
        let name = result.torrent.name.clone();
        let pb = mp.add(ProgressBar::new(result.torrent.length as u64));
        pb.set_style(style.clone());
        pb.set_message(name);
        join.spawn(drive_progress(result, pb));
    }

    let downloads = async {
        while let Some(res) = join.join_next().await {
            res.map_err(|err| anyhow::anyhow!("download task failed: {err}"))??;
        }
        anyhow::Ok(())
    };

    tokio::select! {
        result = downloads => {
            result?;
            if cli.stay_alive() {
                eprintln!("Seeding. Press Ctrl-C to stop.");
                shutdown_signal().await;
            }
            shutdown_session(&session).await;
        }
        _ = shutdown_signal() => {
            shutdown_session(&session).await;
        }
    }

    Ok(())
}

fn add_options(
    input: &str,
    download_dir: &Path,
    verify: bool,
) -> anyhow::Result<AddTorrentOptions> {
    if input.starts_with("magnet:") {
        anyhow::bail!("magnet links are not supported yet");
    }
    let opts = AddTorrentOptions::from_path(input)
        .with_context(|| format!("failed to open torrent {input}"))?;
    let output = util::paths::expand_tilde(download_dir).join(opts.name());
    Ok(opts.verify(verify).output_dir(output))
}

fn progress_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}][{msg}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec},{eta})",
    )
    .unwrap()
    .with_key(
        "eta",
        |state: &ProgressState, w: &mut dyn Write| {
            write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap()
        },
    )
    .progress_chars("#>-")
}

async fn drive_progress(result: AddTorrentResult, pb: ProgressBar) -> anyhow::Result<()> {
    let torrent = result.torrent;
    let mut have = HashSet::new();
    let mut downloaded = 0u64;
    for piece in &result.already_have {
        have.insert(piece.index);
        downloaded += u64::from(piece.length);
    }
    pb.set_position(downloaded);

    while have.len() < torrent.piece_hashes.len() {
        let piece = result.pr_rx.recv_async().await?;
        if have.insert(piece.index) {
            downloaded += u64::from(piece.length);
            pb.set_position(downloaded);
        }
    }
    pb.finish_with_message(format!("{} complete", torrent.name));
    Ok(())
}

async fn shutdown_session(session: &Session) {
    eprintln!("Shutting down...");
    session.shutdown_graceful().await;
    tokio::select! {
        _ = shutdown_signal() => {
            eprintln!("Forced exit");
            std::process::exit(130);
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(8)) => {}
    }
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
