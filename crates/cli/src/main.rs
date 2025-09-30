use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    terminal::{self},
};
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use std::{
    collections::HashMap,
    fmt::Write,
    io::SeekFrom,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};
use tokio::{
    fs::{create_dir_all, File},
    io::{AsyncSeekExt, AsyncWriteExt},
    signal,
};
use tokio_util::sync::CancellationToken;
use tracing::trace;

use bit_rev::{
    session::{DownloadState, PieceResult, Session},
    utils,
};

fn graceful_shutdown() {
    let _ = terminal::disable_raw_mode();
    println!("\n\nShutting down gracefully...");
    std::process::exit(0);
}

#[tokio::main]
async fn main() {
    #[cfg(not(feature = "tokio-console"))]
    tracing_subscriber::fmt::init();

    #[cfg(feature = "tokio-console")]
    console_subscriber::init();

    let filename = std::env::args().nth(1).expect("No torrent path given");
    let output = std::env::args().nth(2);

    if let Err(err) = download_file(&filename, output).await {
        let _ = terminal::disable_raw_mode();
        eprintln!("Error: {:?}", err);
    }
}

pub async fn download_file(filename: &str, out_file: Option<String>) -> anyhow::Result<()> {
    let session = Arc::new(Session::new());
    let shutdown_token = CancellationToken::new();

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
    let session_clone = session.clone();

    // Spawn progress update task
    tokio::spawn(async move {
        loop {
            let new = total_downloaded_clone.load(std::sync::atomic::Ordering::Relaxed);
            pb.set_position(new);
            let status = match session_clone.get_download_state() {
                DownloadState::Init => "Initializing",
                DownloadState::Downloading => "Downloading",
                DownloadState::Paused => "Paused",
            };
            pb.set_message(status);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });

    // Enable raw mode for single keypress detection
    terminal::enable_raw_mode().expect("Failed to enable raw mode");

    // Set up Ctrl+C signal handler
    let _shutdown_token_signal = shutdown_token.clone();
    tokio::spawn(async move {
        let mut sigint = signal::unix::signal(signal::unix::SignalKind::interrupt())
            .expect("Failed to install SIGINT handler");

        sigint.recv().await;
        graceful_shutdown();
    });

    // Spawn keyboard input handler
    let session_input = session.clone();
    let shutdown_token_input = shutdown_token.clone();
    tokio::spawn(async move {
        loop {
            // Check for cancellation
            if shutdown_token_input.is_cancelled() {
                break;
            }

            if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                if let Ok(Event::Key(key_event)) = event::read() {
                    match key_event.code {
                        KeyCode::Char('p') | KeyCode::Char('P') => {
                            match session_input.get_download_state() {
                                DownloadState::Paused => {
                                    session_input.resume();
                                }
                                DownloadState::Downloading => {
                                    session_input.pause();
                                }
                                DownloadState::Init => {
                                    println!("\r\nCannot pause during initialization");
                                }
                            }
                        }
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            graceful_shutdown();
                        }
                        KeyCode::Char('c')
                            if key_event.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            graceful_shutdown();
                        }
                        _ => {}
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let mut hashset = std::collections::HashSet::new();
    let mut pending_pieces: Vec<_> = Vec::new(); // Queue for pieces received while paused

    while hashset.len() < torrent.piece_hashes.len() {
        // Check for shutdown signal
        if shutdown_token.is_cancelled() {
            break;
        }
        // Process any pending pieces first if we're now downloading
        if session.get_download_state() == DownloadState::Downloading && !pending_pieces.is_empty()
        {
            let pieces_to_process = std::mem::take(&mut pending_pieces);
            for pr in pieces_to_process {
                process_piece(
                    &pr,
                    &torrent,
                    &mut file_handles,
                    &mut hashset,
                    &total_downloaded,
                )
                .await?;
            }
        }

        // Use a timeout to periodically check if we should process pending pieces
        let pr_result = tokio::time::timeout(
            Duration::from_millis(100),
            add_torrent_result.pr_rx.recv_async(),
        )
        .await;

        match pr_result {
            Ok(Ok(pr)) => {
                // If paused, queue the piece but don't process it yet
                if session.get_download_state() != DownloadState::Downloading {
                    pending_pieces.push(pr);
                    continue;
                }

                // Process piece immediately if downloading
                process_piece(
                    &pr,
                    &torrent,
                    &mut file_handles,
                    &mut hashset,
                    &total_downloaded,
                )
                .await?;
            }
            Ok(Err(_)) => {
                // Channel closed
                break;
            }
            Err(_) => {
                // Timeout - continue loop to check pending pieces
                continue;
            }
        }
    }

    // Process any remaining pending pieces at the end
    for pr in pending_pieces {
        process_piece(
            &pr,
            &torrent,
            &mut file_handles,
            &mut hashset,
            &total_downloaded,
        )
        .await?;
    }

    // Sync all files
    for (_, file) in file_handles {
        file.sync_all().await?;
    }

    // Restore terminal on completion
    let _ = terminal::disable_raw_mode();
    println!("\nDownload completed!");

    Ok(())
}

async fn process_piece(
    pr: &PieceResult,
    torrent: &bit_rev::torrent::Torrent,
    file_handles: &mut HashMap<usize, File>,
    hashset: &mut std::collections::HashSet<u32>,
    total_downloaded: &Arc<AtomicU64>,
) -> anyhow::Result<bool> {
    hashset.insert(pr.index);

    // Map piece to files and write data accordingly
    let file_mappings = utils::map_piece_to_files(torrent, pr.index as usize);
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
    Ok(true)
}
