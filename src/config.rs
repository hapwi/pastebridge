use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::protocol::DEFAULT_PORT;

pub const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_PRIVATE_FILE_BYTES: u64 = 4 * 1024 * 1024;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device_name: Option<String>,
    pub bind_address: IpAddr,
    pub port: u16,
    pub pair_port: u16,
    pub max_payload_bytes: usize,
    pub poll_interval_ms: u64,
    pub sync_images: bool,
    /// Seconds before an unchanged remotely received clipboard is cleared. Zero disables expiry.
    pub clipboard_ttl_seconds: u64,
    /// Extra host:port entries to try (Tailscale, VPN, etc.)
    pub static_peers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: None,
            bind_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: DEFAULT_PORT,
            pair_port: crate::protocol::PAIR_PORT,
            max_payload_bytes: MAX_PAYLOAD_BYTES,
            poll_interval_ms: 400,
            sync_images: true,
            clipboard_ttl_seconds: 180,
            static_peers: Vec::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<(Self, Paths)> {
        let paths = Paths::new()?;
        fs::create_dir_all(&paths.config_dir)?;
        #[cfg(unix)]
        fs::set_permissions(&paths.config_dir, fs::Permissions::from_mode(0o700))?;
        let cfg = if paths.config_file.exists() {
            let text = read_private_file(&paths.config_file)
                .with_context(|| format!("reading {}", paths.config_file.display()))?;
            toml::from_str(&text).context("parsing config.toml")?
        } else {
            let cfg = Config::default();
            write_secret_file(&paths.config_file, &toml::to_string_pretty(&cfg)?)?;
            cfg
        };
        cfg.validate()?;
        Ok((cfg, paths))
    }

    pub fn device_name(&self) -> String {
        if let Some(name) = &self.device_name {
            if !name.trim().is_empty() {
                return name.trim().to_string();
            }
        }
        hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "pastebridge".into())
    }

    fn validate(&self) -> Result<()> {
        if !(1..=MAX_PAYLOAD_BYTES).contains(&self.max_payload_bytes) {
            anyhow::bail!("max_payload_bytes must be between 1 and {MAX_PAYLOAD_BYTES}");
        }
        if self.clipboard_ttl_seconds > 31_536_000 {
            anyhow::bail!("clipboard_ttl_seconds must be 0 or no more than 31536000");
        }
        if self.static_peers.len() > 64
            || self.static_peers.iter().any(|peer| {
                peer.is_empty() || peer.len() > 253 || peer.chars().any(char::is_control)
            })
        {
            anyhow::bail!("static_peers contains too many or invalid addresses");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub identity_file: PathBuf,
    pub peers_file: PathBuf,
    pub status_file: PathBuf,
    pub pid_file: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let config_dir = if let Ok(dir) = std::env::var("PASTEBRIDGE_HOME") {
            PathBuf::from(dir)
        } else {
            let dirs = ProjectDirs::from("dev", "pastebridge", "pastebridge")
                .context("cannot resolve config directory")?;
            dirs.config_dir().to_path_buf()
        };
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            identity_file: config_dir.join("identity.json"),
            peers_file: config_dir.join("peers.json"),
            status_file: config_dir.join("status.json"),
            pid_file: config_dir.join("pastebridge.pid"),
            config_dir,
        })
    }

    pub fn runtime_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(dir)
        } else {
            std::env::temp_dir()
        }
    }
}

pub fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        anyhow::bail!("refusing to write through symlink {}", path.display());
    }

    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".pastebridge-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temp)?;
        file.write_all(contents.as_bytes())?;
        file.flush()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

pub fn read_private_file(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("refusing to read symlink {}", path.display());
    }
    if !metadata.is_file() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    if metadata.len() > MAX_PRIVATE_FILE_BYTES {
        anyhow::bail!("{} exceeds the private file size limit", path.display());
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let mut text = String::new();
    fs::File::open(path)?
        .take(MAX_PRIVATE_FILE_BYTES.saturating_add(1))
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_PRIVATE_FILE_BYTES {
        anyhow::bail!("{} exceeds the private file size limit", path.display());
    }
    Ok(text)
}

pub fn parse_addr(s: &str, default_port: u16) -> Result<SocketAddr> {
    if s.contains(':') {
        s.parse().with_context(|| format!("invalid address {s}"))
    } else {
        format!("{s}:{default_port}")
            .parse()
            .with_context(|| format!("invalid address {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn legacy_config_gets_safe_clipboard_ttl_default() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.clipboard_ttl_seconds, 180);
    }

    #[test]
    fn clipboard_ttl_can_be_disabled_but_is_bounded() {
        let mut config = Config {
            clipboard_ttl_seconds: 0,
            ..Config::default()
        };
        assert!(config.validate().is_ok());

        config.clipboard_ttl_seconds = 31_536_001;
        assert!(config.validate().is_err());
    }
}
