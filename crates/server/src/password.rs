use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use bit_rev::config::ServerConfig;
use rand::Rng;
use serde::{Deserialize, Serialize};

pub const AUTH_FILE_NAME: &str = "server-auth.toml";
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const GENERATED_LEN: usize = 24;

#[derive(Debug, Serialize, Deserialize)]
struct AuthFile {
    password: String,
}

pub fn auth_file_path(state_dir: &Path) -> PathBuf {
    state_dir.join(AUTH_FILE_NAME)
}

/// Password the daemon should accept.
///
/// A non-empty `config.password` wins. An empty password loads
/// `<state_dir>/server-auth.toml` when that file exists, or generates 24
/// characters, writes the file mode 0600, and prints the password once.
pub fn ensure_password(config: &ServerConfig, state_dir: &Path) -> anyhow::Result<String> {
    if !config.password.is_empty() {
        return Ok(config.password.clone());
    }

    let path = auth_file_path(state_dir);
    if path.exists() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed: AuthFile = toml::from_str(&text)
            .with_context(|| format!("invalid auth file {}", path.display()))?;
        if parsed.password.is_empty() {
            anyhow::bail!("empty password in {}", path.display());
        }
        return Ok(parsed.password);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let password = generate_password(GENERATED_LEN);
    write_secret(&path, &password)?;
    eprintln!(
        "generated server password (saved to {}): {password}",
        path.display()
    );
    Ok(password)
}

pub fn generate_password(len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let index = rng.gen_range(0..ALPHABET.len());
            ALPHABET[index] as char
        })
        .collect()
}

fn write_secret(path: &Path, password: &str) -> anyhow::Result<()> {
    let body = toml::to_string(&AuthFile {
        password: password.to_string(),
    })
    .context("failed to encode server-auth.toml")?;
    let tmp = path.with_extension("toml.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    {
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(body.as_bytes())
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set mode on {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename auth file to {}", path.display()))?;
    Ok(())
}

/// Compare two secrets without short-circuiting on the first differing byte.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    let len = left.len().max(right.len());
    for index in 0..len {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_checks_length_and_bytes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn configured_password_skips_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            password: "from-config".to_string(),
            ..ServerConfig::default()
        };
        let password = ensure_password(&config, dir.path()).unwrap();
        assert_eq!(password, "from-config");
        assert!(!auth_file_path(dir.path()).exists());
    }

    #[test]
    fn generated_password_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig::default();
        let first = ensure_password(&config, dir.path()).unwrap();
        let second = ensure_password(&config, dir.path()).unwrap();
        assert_eq!(first.len(), GENERATED_LEN);
        assert_eq!(first, second);
        assert!(first.chars().all(|ch| ch.is_ascii_alphanumeric()));
    }

    #[cfg(unix)]
    #[test]
    fn generated_password_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        ensure_password(&ServerConfig::default(), dir.path()).unwrap();
        let mode = std::fs::metadata(auth_file_path(dir.path()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
