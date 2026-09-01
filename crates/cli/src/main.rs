use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use std::{
    fmt::Write,
    sync::{atomic::AtomicU64, Arc},
};

use bit_rev::session::{AddTorrentOptions, Session};

#[tokio::main]
async fn main() {
    #[cfg(not(feature = "tokio-console"))]
    tracing_subscriber::fmt::init();

    #[cfg(feature = "tokio-console")]
    console_subscriber::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let verify = args.iter().any(|arg| arg == "--verify");
    let positional: Vec<&str> = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .collect();
    let filename = positional.first().copied().expect("No torrent path given");
    let output = positional.get(1).map(|s| (*s).to_string());

    if let Err(err) = download_file(filename, output, verify).await {
        eprintln!("Error: {:?}", err);
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

pub async fn download_file(
    filename: &str,
    out_file: Option<String>,
    verify: bool,
) -> anyhow::Result<()> {
    let session = Session::new();

    let mut add_opts = AddTorrentOptions::from(filename).verify(verify);
    if let Some(output) = out_file {
        add_opts = add_opts.output_dir(output);
    }

    let add_torrent_result = session.add_torrent(add_opts).await?;
    let torrent = add_torrent_result.torrent.clone();

    let total_size = torrent.length as u64;
    let pb = ProgressBar::new(total_size);

    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{elapsed_precise}][{msg}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec},{eta})"
            ).unwrap().with_key(
            "eta",
            | state: &ProgressState, w: &mut dyn Write | write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap()
        ).progress_chars("#>-")
    );

    let total_downloaded = Arc::new(AtomicU64::new(0));
    let total_downloaded_clone = total_downloaded.clone();

    tokio::spawn(async move {
        loop {
            let new = total_downloaded_clone.load(std::sync::atomic::Ordering::Relaxed);
            pb.set_position(new);
            pb.set_message("Downloading");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    let mut hashset = std::collections::HashSet::new();
    for pr in &add_torrent_result.already_have {
        hashset.insert(pr.index);
        total_downloaded.fetch_add(pr.length as u64, std::sync::atomic::Ordering::Relaxed);
    }

    let download = async {
        while hashset.len() < torrent.piece_hashes.len() {
            let pr = add_torrent_result.pr_rx.recv_async().await?;
            hashset.insert(pr.index);
            total_downloaded.fetch_add(pr.length as u64, std::sync::atomic::Ordering::Relaxed);
        }
        anyhow::Ok(())
    };

    tokio::select! {
        result = download => {
            result?;
            session.shutdown_graceful().await;
        }
        _ = shutdown_signal() => {
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
    }

    Ok(())
}
