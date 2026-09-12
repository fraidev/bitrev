use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::dht::DhtOptions;
use crate::file::AnnounceParams;
use crate::session::{
    SessionOptions, DEFAULT_LISTEN_PORT, DEFAULT_MAX_PEERS_GLOBAL, DEFAULT_MAX_PEERS_PER_TORRENT,
};

/// Session and engine settings loaded from TOML, env, and CLI flags.
///
/// Unknown keys are rejected (`deny_unknown_fields`) so a typo fails with the
/// offending key name in the error. Keys for features that are not implemented
/// yet still parse so config files stay stable across releases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub download_dir: PathBuf,
    pub port: u16,
    pub max_peers_per_torrent: usize,
    pub max_connections: usize,
    pub numwant: u32,
    pub state_dir: PathBuf,
    pub seed: SeedConfig,
    pub dht: DhtConfig,
    pub encryption: EncryptionMode,
    pub utp: UtpConfig,
    pub upload_limit: u64,
    pub download_limit: u64,
    pub pex: bool,
    pub lpd: bool,
    pub nat: bool,
    pub webseed: bool,
    pub ipv6: bool,
    pub queue: QueueConfig,
    pub completed_dir: PathBuf,
    pub watch_dir: PathBuf,
    pub ip_filter: IpFilterConfig,
    pub server: ServerConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SeedConfig {
    pub enabled: bool,
    pub ratio_limit: f64,
    /// Minutes after completion. 0 is unlimited.
    pub time_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DhtConfig {
    pub enabled: bool,
    pub port: u16,
}

/// MSE handshake policy (spec 10). Stored for config stability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionMode {
    Disabled,
    PreferPlaintext,
    #[default]
    PreferEncrypted,
    RequireEncrypted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UtpConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    pub max_active_downloads: usize,
    pub max_active_uploads: usize,
    pub max_active: usize,
    pub dont_count_slow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IpFilterConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub qbittorrent_compat: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            download_dir: PathBuf::from("."),
            port: DEFAULT_LISTEN_PORT,
            max_peers_per_torrent: DEFAULT_MAX_PEERS_PER_TORRENT,
            max_connections: DEFAULT_MAX_PEERS_GLOBAL,
            numwant: AnnounceParams::DEFAULT_NUMWANT,
            state_dir: PathBuf::from("~/.bitrev"),
            seed: SeedConfig::default(),
            dht: DhtConfig::default(),
            encryption: EncryptionMode::default(),
            utp: UtpConfig::default(),
            upload_limit: 0,
            download_limit: 0,
            pex: true,
            lpd: true,
            nat: true,
            webseed: true,
            ipv6: true,
            queue: QueueConfig::default(),
            completed_dir: PathBuf::new(),
            watch_dir: PathBuf::new(),
            ip_filter: IpFilterConfig::default(),
            server: ServerConfig::default(),
        }
    }
}

impl Default for SeedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ratio_limit: 0.0,
            time_limit: 0,
        }
    }
}

impl Default for DhtConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_LISTEN_PORT,
        }
    }
}

impl Default for UtpConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_active_downloads: 5,
            max_active_uploads: 8,
            max_active: 0,
            dont_count_slow: true,
        }
    }
}

impl Default for IpFilterConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8080,
            username: "admin".to_string(),
            password: String::new(),
            qbittorrent_compat: true,
        }
    }
}

impl Config {
    /// Commented default file written by `bitrev config init`.
    pub fn documented_default_toml() -> &'static str {
        include_str!("config.default.toml")
    }

    /// Single mapping from file/env/flag config onto engine session knobs.
    /// Later issues extend both sides of this function.
    pub fn session_options(&self) -> SessionOptions {
        let state_dir = util::paths::expand_tilde(&self.state_dir);
        SessionOptions {
            listen_port: self.port,
            max_peers_per_torrent: self.max_peers_per_torrent,
            max_peers_global: self.max_connections,
            state_dir: if state_dir.as_os_str().is_empty() {
                None
            } else {
                Some(state_dir)
            },
            dht: DhtOptions {
                enabled: self.dht.enabled,
                port: self.dht.port,
                bootstrap_nodes: if self.dht.enabled {
                    DhtOptions::default_bootstrap()
                } else {
                    Vec::new()
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_default_toml_parses_to_default() {
        let parsed: Config = toml::from_str(Config::documented_default_toml()).unwrap();
        assert_eq!(parsed, Config::default());
    }

    #[test]
    fn unknown_key_is_rejected_by_name() {
        let err = toml::from_str::<Config>("not_a_real_key = 1\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not_a_real_key"),
            "error should name the unknown key: {msg}"
        );
    }

    #[test]
    fn unknown_nested_key_is_rejected_by_name() {
        let err = toml::from_str::<Config>("[dht]\nnot_a_dht_key = true\n").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not_a_dht_key"),
            "error should name the unknown key: {msg}"
        );
    }

    #[test]
    fn session_options_maps_engine_keys() {
        let config = Config {
            port: 6999,
            max_peers_per_torrent: 10,
            max_connections: 20,
            state_dir: PathBuf::from("~/.bitrev"),
            ..Config::default()
        };

        let options = config.session_options();
        assert_eq!(options.listen_port, 6999);
        assert_eq!(options.max_peers_per_torrent, 10);
        assert_eq!(options.max_peers_global, 20);
        assert_eq!(
            options.state_dir.as_deref(),
            Some(util::paths::state_dir().as_path())
        );
    }

    #[test]
    fn default_session_options_match_engine_defaults() {
        let options = Config::default().session_options();
        let expected = SessionOptions::default();
        assert_eq!(options.listen_port, expected.listen_port);
        assert_eq!(
            options.max_peers_per_torrent,
            expected.max_peers_per_torrent
        );
        assert_eq!(options.max_peers_global, expected.max_peers_global);
        assert_eq!(options.state_dir, expected.state_dir);
    }

    #[test]
    fn session_options_maps_dht() {
        let options = Config::default().session_options();
        assert!(options.dht.enabled);
        assert_eq!(options.dht.port, DEFAULT_LISTEN_PORT);
        assert_eq!(options.dht.bootstrap_nodes, DhtOptions::default_bootstrap());

        let disabled = Config {
            dht: DhtConfig {
                enabled: false,
                port: 6999,
            },
            ..Config::default()
        }
        .session_options();
        assert!(!disabled.dht.enabled);
        assert_eq!(disabled.dht.port, 6999);
        assert!(disabled.dht.bootstrap_nodes.is_empty());
    }

    #[test]
    fn empty_state_dir_disables_persistence() {
        let config = Config {
            state_dir: PathBuf::new(),
            ..Config::default()
        };
        assert_eq!(config.session_options().state_dir, None);
    }

    #[test]
    fn encryption_modes_round_trip() {
        for mode in [
            EncryptionMode::Disabled,
            EncryptionMode::PreferPlaintext,
            EncryptionMode::PreferEncrypted,
            EncryptionMode::RequireEncrypted,
        ] {
            let config = Config {
                encryption: mode,
                ..Config::default()
            };
            let text = toml::to_string(&config).unwrap();
            let parsed: Config = toml::from_str(&text).unwrap();
            assert_eq!(parsed.encryption, mode);
        }
    }
}
