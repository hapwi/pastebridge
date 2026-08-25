use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Paths;
use crate::ui;

const REPO: &str = "hapwi/pastebridge";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u32, u32, u32);

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

pub fn run(yes: bool, paths: &Paths) -> Result<()> {
    let checking = ui::spinner("checking");
    let latest = latest_release();
    ui::stop(checking);
    let (version, release) = latest?;

    println!();
    let current = parse_version(CURRENT).context("current version is not semver")?;
    if version <= current {
        println!("  {CURRENT}");
        println!("  up to date");
        println!();
        return Ok(());
    }

    println!("  {CURRENT} → {version}");
    println!();
    if !should_update(yes)? {
        return Ok(());
    }

    let target = rustc_target()?;
    let archive = format!("pastebridge-{target}.tar.gz");
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == archive)
        .with_context(|| format!("no build for {target}"))?;
    let sums_url = release
        .assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .map(|asset| asset.browser_download_url.as_str());

    let tmp = TempDir::create()?;
    let archive_path = tmp.0.join(&archive);
    let downloading = ui::spinner("downloading");
    let downloaded = curl_file(&asset.browser_download_url, &archive_path);
    ui::stop(downloading);
    downloaded?;

    if let Some(url) = sums_url {
        let sums = String::from_utf8(curl_get(url)?).unwrap_or_default();
        if let Some(expected) = checksum_for(&sums, &archive) {
            let actual = sha256_file(&archive_path)?;
            if actual != expected {
                bail!("checksum mismatch for {archive}");
            }
        }
    }

    let extracted = tmp.0.join("pastebridge");
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&tmp.0)
        .status()
        .context("tar is required to update")?;
    if !status.success() {
        bail!("could not extract {archive}");
    }
    if !extracted.is_file() {
        bail!("archive did not contain pastebridge");
    }

    replace_binary(&extracted)?;
    if let Err(err) = crate::macos_identity::prepare_executable(
        &std::env::current_exe()?.canonicalize()?,
        &paths.config_dir,
    ) {
        tracing::warn!("could not keep macOS permissions across this update: {err}");
    }
    if crate::daemon::running_pid(paths).is_some() {
        let _ = crate::service::restart();
    }

    println!("  updated to {version}");
    println!();
    Ok(())
}

fn should_update(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal; pass -y to update");
    }
    Ok(ui::confirm("update?"))
}

fn latest_release() -> Result<(Version, Release)> {
    let body = curl_get(&format!(
        "https://api.github.com/repos/{REPO}/releases?per_page=20"
    ))?;
    let releases: Vec<Release> =
        serde_json::from_slice(&body).context("GitHub returned an unexpected response")?;
    releases
        .into_iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| parse_version(&release.tag_name).map(|version| (version, release)))
        .max_by_key(|(version, _)| *version)
        .context("no versioned Pastebridge release found")
}

fn rustc_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        (os, arch) => bail!("no prebuilt Pastebridge binary for {os}/{arch}"),
    }
}

fn replace_binary(new_bin: &Path) -> Result<()> {
    let dest = std::env::current_exe()
        .context("could not locate this pastebridge binary")?
        .canonicalize()
        .context("could not locate this pastebridge binary")?;
    let tmp = dest.with_file_name(format!(
        ".{}.new",
        dest.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("pastebridge")
    ));
    fs::copy(new_bin, &tmp).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&tmp, &dest).with_context(|| format!("replacing {}", dest.display()))?;
    Ok(())
}

fn curl_get(url: &str) -> Result<Vec<u8>> {
    let agent = format!("pastebridge/{CURRENT}");
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "-A",
            &agent,
            "-H",
            "Accept: application/vnd.github+json",
            "--max-time",
            "30",
            url,
        ])
        .output()
        .context("curl is required to update")?;
    if !output.status.success() {
        bail!("could not reach GitHub");
    }
    Ok(output.stdout)
}

fn curl_file(url: &str, dest: &Path) -> Result<()> {
    let agent = format!("pastebridge/{CURRENT}");
    let output = Command::new("curl")
        .args(["-fsSL", "-A", &agent, "--max-time", "120", "-o"])
        .arg(dest)
        .arg(url)
        .output()
        .context("curl is required to update")?;
    if !output.status.success() {
        bail!("download failed");
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn checksum_for(sums: &str, name: &str) -> Option<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        if file == name {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn parse_version(s: &str) -> Option<Version> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version(major, minor, patch))
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn create() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("pastebridge-update-{}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_order_versions() {
        assert_eq!(parse_version("v0.1.1"), Some(Version(0, 1, 1)));
        assert_eq!(parse_version("0.1.2"), Some(Version(0, 1, 2)));
        assert!(parse_version("stable").is_none());
        assert!(Version(0, 1, 1) < Version(0, 1, 2));
        assert!(Version(0, 1, 2) <= Version(0, 1, 2));
    }

    #[test]
    fn reads_sha256sums_lines() {
        let sums = "abc123  pastebridge-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            checksum_for(sums, "pastebridge-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("abc123")
        );
    }
}
