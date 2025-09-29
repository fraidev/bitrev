use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use std::{
    collections::HashMap,
    fmt::Write,
    io::SeekFrom,
    sync::{atomic::AtomicU64, Arc},
};
use tokio::{
    fs::{create_dir_all, File},
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
    let _torrent_meta = add_torrent_result.torrent_meta;

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

    // Determine the output directory
    let output_dir = match out_file {
        Some(name) => name,
        None => torrent.name.clone(),
    };

    // Create output directory and prepare file handles
    let mut file_handles: HashMap<usize, File> = HashMap::new();

    // Create directories and prepare files for multi-file torrents
    for (file_index, file_info) in torrent.files.iter().enumerate() {
        let file_path = if torrent.files.len() == 1 {
            // Single file torrent - use output_dir as filename
            std::path::PathBuf::from(&output_dir)
        } else {
            // Multi-file torrent - create subdirectory structure
            let mut path = std::path::PathBuf::from(&output_dir);
            for component in &file_info.path {
                path.push(component);
            }
            path
        };

        // Create parent directories if needed
        if let Some(parent) = file_path.parent() {
            create_dir_all(parent).await?;
        }

        // Create the file
        let file = File::create(&file_path).await?;
        file_handles.insert(file_index, file);

        trace!("Created file: {:?}", file_path);
    }

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

        // Map piece to files and write data accordingly
        let file_mappings = utils::map_piece_to_files(&torrent, pr.index as usize);
        let mut piece_offset = 0;

        for mapping in file_mappings {
            let file = file_handles.get_mut(&mapping.file_index).ok_or_else(|| {
                anyhow::anyhow!("File handle not found for index {}", mapping.file_index)
            })?;

            // Seek to correct position in file
            file.seek(SeekFrom::Start(mapping.file_offset as u64))
                .await?;

            // Write the portion of the piece that belongs to this file
            let piece_data = &pr.buf[piece_offset..piece_offset + mapping.length];
            file.write_all(piece_data).await?;

            piece_offset += mapping.length;

            trace!(
                "Wrote {} bytes to file {} at offset {}",
                mapping.length,
                mapping.file_index,
                mapping.file_offset
            );
        }

        total_downloaded.fetch_add(pr.length as u64, std::sync::atomic::Ordering::Relaxed);
    }

    // Sync all files
    for (_, file) in file_handles {
        file.sync_all().await?;
    }

    Ok(())
}
