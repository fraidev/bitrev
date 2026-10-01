use std::ffi::OsStr;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::Context;
use bit_rev::create::{create_torrent_file, CreateOptions};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::args::CreateArgs;

pub async fn run(args: CreateArgs) -> anyhow::Result<()> {
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| default_output_path(&args.path));
    let mut opts = CreateOptions::new(&args.path)
        .piece_length(args.piece_length)
        .private(args.private);
    if let Some(comment) = args.comment {
        opts = opts.comment(comment);
    }
    if let Some(name) = args.name {
        opts = opts.name(name);
    }
    for url in args.announce {
        opts = opts.announce_url(url);
    }
    for url in args.web_seed {
        opts = opts.web_seed(url);
    }

    let bar = progress_bar();
    let tick = bar.clone();
    opts = opts.progress(move |done, total| {
        tick.set_length(total);
        tick.set_position(done);
    });

    let output_path = output.clone();
    let source = args.path.display().to_string();
    tokio::task::spawn_blocking(move || create_torrent_file(opts, &output_path))
        .await
        .context("create torrent task failed")?
        .with_context(|| format!("failed to create torrent from {source}"))?;
    bar.finish_and_clear();
    println!("{}", output.display());
    Ok(())
}

fn default_output_path(source: &Path) -> PathBuf {
    let base = source.file_name().unwrap_or_else(|| OsStr::new("torrent"));
    let mut file_name = base.to_os_string();
    file_name.push(".torrent");
    match source.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(file_name),
        _ => PathBuf::from(file_name),
    }
}

fn progress_bar() -> ProgressBar {
    let target = if std::io::stderr().is_terminal() {
        ProgressDrawTarget::stderr()
    } else {
        ProgressDrawTarget::hidden()
    };
    let bar = ProgressBar::with_draw_target(Some(0), target);
    bar.set_style(
        ProgressStyle::with_template("hashing [{bar:40.cyan/blue}] {bytes}/{total_bytes}")
            .unwrap()
            .progress_chars("#>-"),
    );
    bar
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::default_output_path;

    #[test]
    fn default_output_is_next_to_the_source() {
        assert_eq!(
            default_output_path(Path::new("bundle")).as_os_str(),
            "bundle.torrent"
        );
        assert_eq!(
            default_output_path(Path::new("/tmp/bundle")).as_os_str(),
            "/tmp/bundle.torrent"
        );
        assert_eq!(
            default_output_path(Path::new("dir/movie.mkv")).as_os_str(),
            "dir/movie.mkv.torrent"
        );
        assert_eq!(
            default_output_path(Path::new("./dir")).as_os_str(),
            "./dir.torrent"
        );
    }
}
