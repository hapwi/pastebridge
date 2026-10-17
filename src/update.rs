use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
        .map(|asset| asset.browser_download_url.as_str())
        .context("release is missing SHA256SUMS; refusing an unverified update")?;

    let tmp = TempDir::create()?;
    let archive_path = tmp.0.join(&archive);
    let downloading = ui::spinner("downloading");
    let downloaded = curl_file(&asset.browser_download_url, &archive_path);
    ui::stop(downloading);
    downloaded?;

    let sums = String::from_utf8(curl_get(sums_url)?).context("invalid checksum manifest")?;
    verify_checksum(&archive_path, &sums, &archive)?;

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
    if !fs::symlink_metadata(&extracted).is_ok_and(|metadata| metadata.file_type().is_file()) {
        bail!("archive did not contain pastebridge");
    }

    verify_binary_version(&extracted, &version.to_string())?;
    let dest = std::env::current_exe()?.canonicalize()?;
    let was_running = crate::daemon::running_pid(paths).is_some();
    activate_binary(
        &extracted,
        &dest,
        || {
            crate::macos_identity::prepare_executable(&dest)?;
            if was_running {
                crate::service::restart()?;
                crate::service::wait_for_ready(
                    paths,
                    &version.to_string(),
                    Duration::from_secs(45),
                )?;
            }
            Ok(())
        },
        || {
            if was_running {
                crate::service::restart()?;
                crate::service::wait_for_ready(paths, CURRENT, Duration::from_secs(45))?;
            }
            Ok(())
        },
    )?;

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

fn verify_checksum(path: &Path, sums: &str, archive: &str) -> Result<()> {
    let expected = checksum_for(sums, archive)
        .with_context(|| format!("SHA256SUMS does not list {archive}"))?;
    if sha256_file(path)? != expected {
        bail!("checksum mismatch for {archive}");
    }
    Ok(())
}

fn verify_binary_version(binary: &Path, expected: &str) -> Result<()> {
    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("the downloaded executable does not run")?;
    let mut stdout = child.stdout.take().context("reading downloaded version")?;
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .by_ref()
            .take(4097)
            .read_to_end(&mut output)
            .map(|_| output)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("the downloaded executable timed out during its version check");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = reader
        .join()
        .map_err(|_| anyhow::anyhow!("version reader stopped"))??;
    if !status.success()
        || String::from_utf8_lossy(&output).trim() != format!("pastebridge {expected}")
    {
        bail!("the downloaded executable did not report Pastebridge {expected}");
    }
    Ok(())
}

fn activate_binary(
    new_bin: &Path,
    dest: &Path,
    activate: impl FnOnce() -> Result<()>,
    recover: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let parent = dest.parent().context("binary has no parent directory")?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(parent.join(".pastebridge-update.lock"))?;
    lock.try_lock_exclusive()
        .context("another Pastebridge update is in progress")?;
    let staging = TempDir::create_in(parent)?;
    let backup = staging.0.join("previous");
    let prepared = staging.0.join("pastebridge");
    fs::copy(dest, &backup).context("saving the previous executable")?;
    fs::File::open(&backup)?.sync_all()?;
    fs::copy(new_bin, &prepared)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&prepared, fs::Permissions::from_mode(0o755))?;
    }
    fs::File::open(&prepared)?.sync_all()?;
    fs::rename(&prepared, dest).context("activating the prepared executable")?;
    if let Err(err) = activate() {
        if let Err(restore) = fs::rename(&backup, dest) {
            let retained = staging.0.clone();
            std::mem::forget(staging);
            bail!("update failed ({err:#}); restore failed ({restore}); previous executable remains at {}", retained.join("previous").display());
        }
        if let Err(recovery) = recover() {
            bail!("update failed ({err:#}); previous executable restored but its service could not restart ({recovery:#})");
        }
        bail!("update failed ({err:#}); rolled back to the previous executable");
    }
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
        let Some(hash) = parts.next() else { continue };
        let Some(file) = parts.next() else { continue };
        if file.trim_start_matches('*') == name
            && hash.len() == 64
            && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            && parts.next().is_none()
        {
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
        Self::create_in(&std::env::temp_dir())
    }

    fn create_in(parent: &Path) -> Result<Self> {
        let path = parent.join(format!(
            ".pastebridge-update-{:032x}",
            rand::random::<u128>()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
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
    fn missing_or_wrong_checksums_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("release.tar.gz");
        fs::write(&archive, b"release bytes").unwrap();
        assert!(verify_checksum(&archive, "", "release.tar.gz").is_err());
        assert!(verify_checksum(&archive, "invalid  release.tar.gz", "release.tar.gz").is_err());
        assert!(verify_checksum(
            &archive,
            &format!("{}  release.tar.gz", "0".repeat(64)),
            "release.tar.gz"
        )
        .is_err());
        let sums = format!("\n{}  release.tar.gz\n", sha256_file(&archive).unwrap());
        verify_checksum(&archive, &sums, "release.tar.gz").unwrap();
    }

    #[test]
    fn unsuccessful_activation_restores_the_previous_executable() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("pastebridge");
        let new = dir.path().join("download");
        fs::write(&dest, b"previous executable").unwrap();
        fs::write(&new, b"new executable").unwrap();
        let result = activate_binary(
            &new,
            &dest,
            || {
                assert_eq!(fs::read(&dest).unwrap(), b"new executable");
                bail!("service did not become ready")
            },
            || {
                assert_eq!(fs::read(&dest).unwrap(), b"previous executable");
                Ok(())
            },
        );
        assert!(result.unwrap_err().to_string().contains("rolled back"));
        assert_eq!(fs::read(&dest).unwrap(), b"previous executable");
        activate_binary(&new, &dest, || Ok(()), || unreachable!()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new executable");
    }

    #[cfg(unix)]
    #[test]
    fn staged_executable_must_report_the_requested_version() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("pastebridge");
        fs::write(&binary, "#!/bin/sh\nprintf 'pastebridge 1.2.3\\n'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        verify_binary_version(&binary, "1.2.3").unwrap();
        assert!(verify_binary_version(&binary, "1.2.4")
            .unwrap_err()
            .to_string()
            .contains("did not report"));
    }

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
        let sums = format!(
            "{}  pastebridge-x86_64-unknown-linux-gnu.tar.gz\n",
            "a".repeat(64)
        );
        assert_eq!(
            checksum_for(&sums, "pastebridge-x86_64-unknown-linux-gnu.tar.gz"),
            Some("a".repeat(64))
        );
    }
}
