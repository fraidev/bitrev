use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use std::{
    fmt::Write,
    io::SeekFrom,
    sync::{atomic::AtomicU64, Arc},
};
use tokio::{
    fs::File,
    io::{AsyncSeekExt, AsyncWriteExt},
};
use tracing::trace;

use bit_rev::{session::Session, utils};

#[tokio::main]
async fn main() {
    #[cfg(not(feature = "tokio-console"))]
    tracing_subscriber::fmt::init();

    #[cfg(feature = "tokio-console")]
    console_subscriber::init();

    let filename = std::env::args().nth(1).expect("No torrent path given");
    let output = std::env::args().nth(2);

    if let Err(err) = download_file(&filename, output).await {
        eprintln!("Error: {:?}", err);
    }
}

pub async fn download_file(filename: &str, out_file: Option<String>) -> anyhow::Result<()> {
    let session = Session::new();

    let add_torrent_result = session.add_torrent(filename.into()).await?;
    let torrent = add_torrent_result.torrent.clone();
    let torrent_meta = add_torrent_result.torrent_meta;

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

    let out_filename = match out_file {
        Some(name) => name,
        None => torrent_meta.clone().torrent_file.info.name.clone(),
    };
    let mut file = File::create(out_filename).await?;

    // File
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
    while hashset.len() < torrent.piece_hashes.len() {
        let pr = add_torrent_result.pr_rx.recv_async().await?;

        hashset.insert(pr.index);
        let (start, end) = utils::calculate_bounds_for_piece(&torrent, pr.index as usize);
        trace!(
            "index: {}, start: {}, end: {} len {}",
            pr.index,
            start,
            end,
            pr.length
        );
        file.seek(SeekFrom::Start(start as u64)).await?;
        file.write_all(pr.buf.as_slice()).await?;

        total_downloaded.fetch_add(pr.length as u64, std::sync::atomic::Ordering::Relaxed);
    }

    file.sync_all().await?;

    Ok(())
}
