use bitrev_cli::args::{init_tracing, Cli, Command, ConfigCommand};
use bitrev_cli::config::{default_config_path, load_config, write_default_config, FlagOverrides};
use bitrev_cli::download;
use clap::Parser;

fn main() {
    let cli = Cli::parse();
    if let Err(err) = run(cli) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Config {
            command: ConfigCommand::Init { force },
        }) => {
            let path = cli.config.clone().unwrap_or_else(default_config_path);
            write_default_config(&path, force)?;
            eprintln!("Wrote {}", path.display());
            Ok(())
        }
        None => {
            if cli.inputs.is_empty() {
                anyhow::bail!("no torrent or magnet given");
            }
            init_tracing(cli.verbose, cli.quiet);
            let config = load_config(
                cli.config.as_deref(),
                |key| std::env::var(key).ok(),
                &FlagOverrides::from(&cli),
            )?;
            download::run(cli, config).await
        }
    }
}
