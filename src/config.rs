use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::protocol::DEFAULT_PORT;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device_name: Option<String>,
    pub port: u16,
    pub pair_port: u16,
    pub max_payload_bytes: usize,
    pub poll_interval_ms: u64,
    pub sync_images: bool,
    /// Extra host:port entries to try (Tailscale, VPN, etc.)
    pub static_peers: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: None,
            port: DEFAULT_PORT,
            pair_port: crate::protocol::PAIR_PORT,
            max_payload_bytes: 8 * 1024 * 1024,
            poll_interval_ms: 400,
            sync_images: true,
            static_peers: Vec::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<(Self, Paths)> {
        let paths = Paths::new()?;
        fs::create_dir_all(&paths.config_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&paths.config_dir, fs::Permissions::from_mode(0o700));
        }
        let cfg = if paths.config_file.exists() {
            let text = fs::read_to_string(&paths.config_file)
                .with_context(|| format!("reading {}", paths.config_file.display()))?;
            toml::from_str(&text).context("parsing config.toml")?
        } else {
            let cfg = Config::default();
            fs::write(&paths.config_file, toml::to_string_pretty(&cfg)?)?;
            cfg
        };
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
    fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
