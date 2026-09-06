use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

/// A BitTorrent client.
#[derive(Debug, Parser)]
#[command(
    name = "bitrev",
    version = bit_rev::identity::CLIENT_VERSION,
    about = "A BitTorrent client",
    arg_required_else_help = true
)]
pub struct Cli {
    /// Torrent files or magnet links
    #[arg(value_name = "TORRENT|MAGNET")]
    pub inputs: Vec<String>,

    /// Download directory
    #[arg(short, long, value_name = "DIR")]
    pub output: Option<PathBuf>,

    /// TCP listen port
    #[arg(short, long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Stay running and seed after downloads complete
    #[arg(long, overrides_with = "no_seed")]
    pub seed: bool,

    /// Exit after downloads complete
    #[arg(long = "no-seed")]
    pub no_seed: bool,

    /// Force a full re-hash instead of trusting resume data
    #[arg(long)]
    pub verify: bool,

    /// Path to the TOML config file
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// Decrease log verbosity
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub quiet: u8,

    /// Increase log verbosity
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect or write configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write a default commented config file
    Init {
        /// Overwrite an existing file
        #[arg(long)]
        force: bool,
    },
}

impl Cli {
    /// `--seed` keeps the process alive. `--no-seed` and the default exit after completion.
    pub fn stay_alive(&self) -> bool {
        self.seed && !self.no_seed
    }
}

pub fn init_tracing(verbose: u8, quiet: u8) {
    #[cfg(feature = "tokio-console")]
    {
        let _ = (verbose, quiet);
        console_subscriber::init();
    }

    #[cfg(not(feature = "tokio-console"))]
    {
        use tracing_subscriber::EnvFilter;

        let default_level = match (verbose as i16).saturating_sub(quiet as i16) {
            ..=-3 => "off",
            -2 => "error",
            -1 => "warn",
            0 => "info",
            1 => "debug",
            _ => "trace",
        };
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::Cli;

    #[test]
    fn version_matches_identity() {
        let cmd = Cli::command();
        assert_eq!(cmd.get_version(), Some(bit_rev::identity::CLIENT_VERSION));
        assert_eq!(env!("CARGO_PKG_VERSION"), bit_rev::identity::CLIENT_VERSION);
    }

    #[test]
    fn help_lists_every_flag() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        cmd.write_long_help(&mut buf).unwrap();
        let help = String::from_utf8(buf).unwrap();

        for needle in [
            "--output",
            "-o",
            "--port",
            "-p",
            "--seed",
            "--no-seed",
            "--verify",
            "--config",
            "--quiet",
            "-q",
            "--verbose",
            "-v",
            "--help",
            "--version",
            "config",
        ] {
            assert!(help.contains(needle), "help is missing {needle}:\n{help}");
        }

        let mut config_cmd = Cli::command();
        let init = config_cmd.find_subcommand_mut("config").unwrap();
        let mut init_help = Vec::new();
        init.write_long_help(&mut init_help).unwrap();
        let init_help = String::from_utf8(init_help).unwrap();
        assert!(
            init_help.contains("init"),
            "config help is missing init:\n{init_help}"
        );

        let mut init_cmd = Cli::command();
        let init = init_cmd
            .find_subcommand_mut("config")
            .unwrap()
            .find_subcommand_mut("init")
            .unwrap();
        let mut force_help = Vec::new();
        init.write_long_help(&mut force_help).unwrap();
        let force_help = String::from_utf8(force_help).unwrap();
        assert!(
            force_help.contains("--force"),
            "config init help is missing --force:\n{force_help}"
        );
    }

    #[test]
    fn parses_repeatable_inputs_and_flags() {
        let cli = Cli::parse_from([
            "bitrev",
            "-o",
            "/tmp/dl",
            "-p",
            "6999",
            "--seed",
            "--verify",
            "--config",
            "/tmp/cfg.toml",
            "one.torrent",
            "two.torrent",
        ]);
        assert_eq!(cli.inputs, ["one.torrent", "two.torrent"]);
        assert_eq!(cli.output.as_deref().unwrap().as_os_str(), "/tmp/dl");
        assert_eq!(cli.port, Some(6999));
        assert!(cli.seed);
        assert!(cli.verify);
        assert!(cli.stay_alive());
    }

    #[test]
    fn no_seed_overrides_seed() {
        let cli = Cli::parse_from(["bitrev", "--seed", "--no-seed", "t.torrent"]);
        assert!(!cli.stay_alive());
    }
}
