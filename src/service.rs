use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::Paths;

pub fn wait_for_ready(paths: &Paths, version: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready_status(paths, version).is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("Pastebridge {version} did not become ready. Run pastebridge doctor and check the service logs")
}

pub fn ready_status(paths: &Paths, version: &str) -> Option<crate::daemon::Status> {
    let pid = crate::daemon::running_pid(paths)?;
    let text = std::fs::read_to_string(&paths.status_file).ok()?;
    let status: crate::daemon::Status = serde_json::from_str(&text).ok()?;
    let modified = std::fs::metadata(&paths.status_file)
        .ok()?
        .modified()
        .ok()?;
    (status.pid == pid
        && status.running
        && status.ready
        && status.version == version
        && modified.elapsed().ok()? < Duration::from_secs(6))
    .then_some(status)
}

pub fn install() -> Result<()> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("install-service is only supported on Linux and macOS");
    let exe = std::env::current_exe()?.canonicalize()?;
    #[cfg(target_os = "linux")]
    {
        install_systemd(&exe)?;
    }
    #[cfg(target_os = "macos")]
    {
        install_launchd(&exe)?;
    }
    wait_for_ready(
        &Paths::new()?,
        env!("CARGO_PKG_VERSION"),
        Duration::from_secs(45),
    )?;
    crate::ui::success("Login service enabled and ready");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        uninstall_systemd()
    }
    #[cfg(target_os = "macos")]
    {
        uninstall_launchd()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("uninstall-service is only supported on Linux and macOS")
    }
}

pub fn restart() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        restart_systemd()
    }
    #[cfg(target_os = "macos")]
    {
        restart_launchd()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("restart is only supported on Linux and macOS")
    }
}

#[cfg(target_os = "linux")]
fn unit_path() -> Result<PathBuf> {
    let dir = dirs_config()?.join("systemd/user");
    fs::create_dir_all(&dir)?;
    Ok(dir.join("pastebridge.service"))
}

#[cfg(target_os = "linux")]
fn dirs_config() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config"))
}

#[cfg(target_os = "linux")]
fn install_systemd(exe: &std::path::Path) -> Result<()> {
    let path = unit_path()?;
    let unit = format!(
        "[Unit]\n\
         Description=Pastebridge clipboard sync\n\
         After=graphical-session-pre.target network-online.target\n\
         Wants=network-online.target\n\
         PartOf=graphical-session.target\n\
         \n\
         [Service]\n\
         ExecStart=\"{exe}\" start\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         Environment=RUST_LOG=pastebridge=info\n\
         PassEnvironment=DISPLAY WAYLAND_DISPLAY XAUTHORITY XDG_RUNTIME_DIR\n\
         NoNewPrivileges=true\n\
         PrivateTmp=true\n\
         RestrictSUIDSGID=true\n\
         RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        exe = exe
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    );
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "pastebridge.service"])
        .output();
    fs::write(&path, unit)?;

    let reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()?;
    if !reload.success() {
        anyhow::bail!("systemctl --user daemon-reload failed");
    }
    let enable = Command::new("systemctl")
        .args(["--user", "enable", "--now", "pastebridge.service"])
        .output()
        .context("running systemctl")?;
    if !enable.status.success() {
        anyhow::bail!(
            "could not enable the login service with systemctl --user: {}",
            String::from_utf8_lossy(&enable.stderr).trim()
        );
    }
    restart_systemd()
}

#[cfg(target_os = "linux")]
fn uninstall_systemd() -> Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "pastebridge.service"])
        .status();
    if let Ok(path) = unit_path() {
        let _ = fs::remove_file(path);
    }
    println!("  login service removed");
    Ok(())
}

#[cfg(target_os = "linux")]
fn restart_systemd() -> Result<()> {
    let status = Command::new("systemctl")
        .args(["--user", "restart", "pastebridge.service"])
        .status()
        .context("running systemctl")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("systemctl restart failed")
    }
}

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let dir = PathBuf::from(home).join("Library/LaunchAgents");
    fs::create_dir_all(&dir)?;
    Ok(dir.join("dev.pastebridge.daemon.plist"))
}

#[cfg(target_os = "macos")]
fn install_launchd(exe: &std::path::Path) -> Result<()> {
    let path = plist_path()?;
    let logs = PathBuf::from(std::env::var("HOME")?).join("Library/Logs");
    fs::create_dir_all(&logs)?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.pastebridge.daemon</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>start</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>pastebridge=info</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{logs}/pastebridge.log</string>
  <key>StandardErrorPath</key>
  <string>{logs}/pastebridge.err</string>
</dict>
</plist>
"#,
        exe = xml_escape(&exe.to_string_lossy()),
        logs = xml_escape(&logs.to_string_lossy())
    );
    fs::write(&path, plist)?;
    if let Err(err) = crate::macos_identity::prepare_executable(exe) {
        tracing::warn!("could not clear macOS quarantine on this binary: {err}");
    }
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let domain = format!("gui/{uid}");
    let label = format!("{domain}/dev.pastebridge.daemon");
    let already = launchd_loaded(&label);
    if already {
        let status = Command::new("launchctl")
            .args(["bootout", &label])
            .status()?;
        if !status.success() {
            anyhow::bail!("could not unload the existing login service");
        }
    }
    let enabled = Command::new("launchctl")
        .args(["enable", &label])
        .status()?;
    if !enabled.success() {
        anyhow::bail!("could not enable the login service with launchctl");
    }
    let load = Command::new("launchctl")
        .args(["bootstrap", &domain, &path.display().to_string()])
        .status()?;
    if !load.success() {
        anyhow::bail!("could not load the login service with launchctl");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn launchd_loaded(label: &str) -> bool {
    Command::new("launchctl")
        .args(["print", label])
        .output()
        .is_ok_and(|out| out.status.success())
}

#[cfg(target_os = "macos")]
fn uninstall_launchd() -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/dev.pastebridge.daemon")])
        .status();
    if let Ok(path) = plist_path() {
        let _ = fs::remove_file(path);
    }
    println!("  login service removed");
    Ok(())
}

#[cfg(target_os = "macos")]
fn restart_launchd() -> Result<()> {
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let status = Command::new("launchctl")
        .args([
            "kickstart",
            "-k",
            &format!("gui/{uid}/dev.pastebridge.daemon"),
        ])
        .status()
        .context("running launchctl")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("launchctl kickstart failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_requires_the_live_pid_version_and_clipboard_worker() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().into(),
            config_file: dir.path().join("config.toml"),
            identity_file: dir.path().join("identity.json"),
            peers_file: dir.path().join("peers.json"),
            pid_file: dir.path().join("pastebridge.pid"),
            status_file: dir.path().join("status.json"),
        };
        fs::write(&paths.pid_file, std::process::id().to_string()).unwrap();
        let mut status = serde_json::json!({
            "running": true, "ready": false, "pid": std::process::id(),
            "version": "1.2.3", "updated_at": "now", "device_id": "test", "name": "test",
            "port": 27419, "backend": "test", "peers": [],
            "last_sent": null, "last_received": null, "last_error": null,
        });
        let save =
            |status: &serde_json::Value| fs::write(&paths.status_file, status.to_string()).unwrap();
        save(&status);
        assert!(ready_status(&paths, "1.2.3").is_none());
        status["ready"] = true.into();
        save(&status);
        assert_eq!(ready_status(&paths, "1.2.3").unwrap().version, "1.2.3");
        assert!(ready_status(&paths, "9.9.9").is_none());
        status["pid"] = 0.into();
        save(&status);
        assert!(ready_status(&paths, "1.2.3").is_none());
    }
}
