use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, Context};
use bit_rev::config::{Config, EncryptionMode};

use crate::args::Cli;

#[derive(Debug, Default, Clone)]
pub struct FlagOverrides {
    pub port: Option<u16>,
    pub download_dir: Option<PathBuf>,
}

impl From<&Cli> for FlagOverrides {
    fn from(cli: &Cli) -> Self {
        Self {
            port: cli.port,
            download_dir: cli.output.clone(),
        }
    }
}

pub fn default_config_path() -> PathBuf {
    util::paths::state_dir().join("config.toml")
}

/// Load config with precedence: defaults < TOML file < `BITREV_*` env < flags.
///
/// A missing file is not an error. A malformed file names the path and key.
pub fn load_config(
    config_path: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
    flags: &FlagOverrides,
) -> anyhow::Result<Config> {
    let path = config_path
        .map(Path::to_path_buf)
        .unwrap_or_else(default_config_path);

    let mut config = if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        parse_config_file(&path, &contents)?
    } else {
        Config::default()
    };

    apply_env(&mut config, &env)?;
    apply_flags(&mut config, flags);
    Ok(config)
}

pub fn parse_config_file(path: &Path, contents: &str) -> anyhow::Result<Config> {
    toml::from_str(contents).map_err(|err| anyhow!("invalid config file {}: {err}", path.display()))
}

pub fn write_default_config(path: &Path, force: bool) -> anyhow::Result<()> {
    if path.exists() && !force {
        anyhow::bail!(
            "config already exists at {}, use --force to overwrite",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }
    std::fs::write(path, Config::documented_default_toml())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn apply_flags(config: &mut Config, flags: &FlagOverrides) {
    if let Some(port) = flags.port {
        config.port = port;
    }
    if let Some(dir) = &flags.download_dir {
        config.download_dir = dir.clone();
    }
}

fn apply_env(config: &mut Config, env: &impl Fn(&str) -> Option<String>) -> anyhow::Result<()> {
    if let Some(v) = env_parse(env, "BITREV_PORT")? {
        config.port = v;
    }
    if let Some(v) = env("BITREV_DOWNLOAD_DIR") {
        config.download_dir = PathBuf::from(v);
    }
    if let Some(v) = env_parse(env, "BITREV_MAX_PEERS_PER_TORRENT")? {
        config.max_peers_per_torrent = v;
    }
    if let Some(v) = env_parse(env, "BITREV_MAX_CONNECTIONS")? {
        config.max_connections = v;
    }
    if let Some(v) = env_parse(env, "BITREV_NUMWANT")? {
        config.numwant = v;
    }
    if let Some(v) = env("BITREV_STATE_DIR") {
        config.state_dir = PathBuf::from(v);
    }
    if let Some(v) = env_bool(env, "BITREV_SEED_ENABLED")? {
        config.seed.enabled = v;
    }
    if let Some(v) = env_parse(env, "BITREV_SEED_RATIO_LIMIT")? {
        config.seed.ratio_limit = v;
    }
    if let Some(v) = env_parse(env, "BITREV_SEED_TIME_LIMIT")? {
        config.seed.time_limit = v;
    }
    if let Some(v) = env_bool(env, "BITREV_DHT_ENABLED")? {
        config.dht.enabled = v;
    }
    if let Some(v) = env_parse(env, "BITREV_DHT_PORT")? {
        config.dht.port = v;
    }
    if let Some(v) = env("BITREV_ENCRYPTION") {
        config.encryption = parse_encryption(&v)?;
    }
    if let Some(v) = env_bool(env, "BITREV_UTP_ENABLED")? {
        config.utp.enabled = v;
    }
    if let Some(v) = env_parse(env, "BITREV_UPLOAD_LIMIT")? {
        config.upload_limit = v;
    }
    if let Some(v) = env_parse(env, "BITREV_DOWNLOAD_LIMIT")? {
        config.download_limit = v;
    }
    if let Some(v) = env_bool(env, "BITREV_PEX")? {
        config.pex = v;
    }
    if let Some(v) = env_bool(env, "BITREV_LPD")? {
        config.lpd = v;
    }
    if let Some(v) = env_bool(env, "BITREV_NAT")? {
        config.nat = v;
    }
    if let Some(v) = env_bool(env, "BITREV_WEBSEED")? {
        config.webseed = v;
    }
    if let Some(v) = env_bool(env, "BITREV_IPV6")? {
        config.ipv6 = v;
    }
    if let Some(v) = env_parse(env, "BITREV_QUEUE_MAX_ACTIVE_DOWNLOADS")? {
        config.queue.max_active_downloads = v;
    }
    if let Some(v) = env_parse(env, "BITREV_QUEUE_MAX_ACTIVE_UPLOADS")? {
        config.queue.max_active_uploads = v;
    }
    if let Some(v) = env_parse(env, "BITREV_QUEUE_MAX_ACTIVE")? {
        config.queue.max_active = v;
    }
    if let Some(v) = env_bool(env, "BITREV_QUEUE_DONT_COUNT_SLOW")? {
        config.queue.dont_count_slow = v;
    }
    if let Some(v) = env("BITREV_COMPLETED_DIR") {
        config.completed_dir = PathBuf::from(v);
    }
    if let Some(v) = env("BITREV_WATCH_DIR") {
        config.watch_dir = PathBuf::from(v);
    }
    if let Some(v) = env("BITREV_IP_FILTER_PATH") {
        config.ip_filter.path = PathBuf::from(v);
    }
    if let Some(v) = env("BITREV_SERVER_HOST") {
        config.server.host = v;
    }
    if let Some(v) = env_parse(env, "BITREV_SERVER_PORT")? {
        config.server.port = v;
    }
    if let Some(v) = env("BITREV_SERVER_USERNAME") {
        config.server.username = v;
    }
    if let Some(v) = env("BITREV_SERVER_PASSWORD") {
        config.server.password = v;
    }
    if let Some(v) = env_bool(env, "BITREV_SERVER_QBITTORRENT_COMPAT")? {
        config.server.qbittorrent_compat = v;
    }
    Ok(())
}

fn env_parse<T: FromStr>(
    env: &impl Fn(&str) -> Option<String>,
    key: &str,
) -> anyhow::Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    match env(key) {
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|err| anyhow!("invalid {key}: {err}")),
        None => Ok(None),
    }
}

fn env_bool(env: &impl Fn(&str) -> Option<String>, key: &str) -> anyhow::Result<Option<bool>> {
    match env(key) {
        Some(value) => parse_bool(key, &value).map(Some),
        None => Ok(None),
    }
}

fn parse_bool(key: &str, value: &str) -> anyhow::Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => anyhow::bail!("invalid boolean for {key}: {value}"),
    }
}

fn parse_encryption(value: &str) -> anyhow::Result<EncryptionMode> {
    match value {
        "disabled" => Ok(EncryptionMode::Disabled),
        "prefer_plaintext" => Ok(EncryptionMode::PreferPlaintext),
        "prefer_encrypted" => Ok(EncryptionMode::PreferEncrypted),
        "require_encrypted" => Ok(EncryptionMode::RequireEncrypted),
        _ => anyhow::bail!("invalid BITREV_ENCRYPTION: {value}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::{load_config, write_default_config, FlagOverrides};

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn flag_overrides_env_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 6882\ndownload_dir = \"/from-file\"\n").unwrap();

        let config = load_config(
            Some(&path),
            env_map(&[
                ("BITREV_PORT", "6883"),
                ("BITREV_DOWNLOAD_DIR", "/from-env"),
            ]),
            &FlagOverrides {
                port: Some(6884),
                download_dir: Some(PathBuf::from("/from-flag")),
            },
        )
        .unwrap();

        assert_eq!(config.port, 6884);
        assert_eq!(config.download_dir, PathBuf::from("/from-flag"));
    }

    #[test]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 6882\ndownload_dir = \"/from-file\"\n").unwrap();

        let config = load_config(
            Some(&path),
            env_map(&[
                ("BITREV_PORT", "6883"),
                ("BITREV_DOWNLOAD_DIR", "/from-env"),
            ]),
            &FlagOverrides::default(),
        )
        .unwrap();

        assert_eq!(config.port, 6883);
        assert_eq!(config.download_dir, PathBuf::from("/from-env"));
    }

    #[test]
    fn env_overrides_without_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");

        let config = load_config(
            Some(&missing),
            env_map(&[
                ("BITREV_PORT", "6999"),
                ("BITREV_DOWNLOAD_DIR", "/from-env"),
            ]),
            &FlagOverrides::default(),
        )
        .unwrap();

        assert_eq!(config.port, 6999);
        assert_eq!(config.download_dir, PathBuf::from("/from-env"));
    }

    #[test]
    fn file_used_when_no_env_or_flags() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = 6882\ndownload_dir = \"/from-file\"\n").unwrap();

        let config = load_config(Some(&path), |_| None, &FlagOverrides::default()).unwrap();
        assert_eq!(config.port, 6882);
        assert_eq!(config.download_dir, PathBuf::from("/from-file"));
    }

    #[test]
    fn nested_env_overrides_dht_and_server_port() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        let config = load_config(
            Some(&missing),
            env_map(&[("BITREV_DHT_PORT", "7001"), ("BITREV_SERVER_PORT", "9090")]),
            &FlagOverrides::default(),
        )
        .unwrap();
        assert_eq!(config.dht.port, 7001);
        assert_eq!(config.server.port, 9090);
    }

    #[test]
    fn malformed_toml_names_file_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "port = \"not-a-port\"\n").unwrap();

        let err = load_config(Some(&path), |_| None, &FlagOverrides::default()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bad.toml") || msg.contains(&path.display().to_string()),
            "error should name the file: {msg}"
        );
        assert!(msg.contains("port"), "error should name the key: {msg}");
    }

    #[test]
    fn unknown_key_is_rejected_with_file_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not_a_real_key = 1\n").unwrap();

        let err = load_config(Some(&path), |_| None, &FlagOverrides::default()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bad.toml") || msg.contains(&path.display().to_string()),
            "error should name the file: {msg}"
        );
        assert!(
            msg.contains("not_a_real_key"),
            "error should name the key: {msg}"
        );
    }

    #[test]
    fn config_init_output_reparses_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_default_config(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: bit_rev::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, bit_rev::config::Config::default());
    }

    #[test]
    fn config_init_refuses_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_default_config(&path, false).unwrap();
        let err = write_default_config(&path, false).unwrap_err();
        assert!(err.to_string().contains("--force"));
        write_default_config(&path, true).unwrap();
    }
}
