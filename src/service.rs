use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

pub fn install() -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    #[cfg(target_os = "linux")]
    {
        install_systemd(&exe)
    }
    #[cfg(target_os = "macos")]
    {
        install_launchd(&exe)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("install-service is only supported on Linux and macOS")
    }
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
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exe} start\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         Environment=RUST_LOG=pastebridge=info\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display()
    );
    fs::write(&path, unit)?;
    println!("Wrote {}", path.display());

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let enable = Command::new("systemctl")
        .args(["--user", "enable", "--now", "pastebridge.service"])
        .status()
        .context("running systemctl")?;
    if enable.success() {
        println!("Enabled user service `pastebridge.service`.");
        println!("It will start whenever you log in.");
        let _ = Command::new("loginctl")
            .args(["enable-linger", &std::env::var("USER").unwrap_or_default()])
            .status();
    } else {
        println!("Could not enable via systemctl. Start it yourself with:");
        println!("  systemctl --user enable --now pastebridge.service");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_systemd() -> Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "pastebridge.service"])
        .status();
    if let Ok(path) = unit_path() {
        let _ = fs::remove_file(path);
    }
    println!("Removed the Pastebridge user service.");
    Ok(())
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
        exe = exe.display(),
        logs = logs.display()
    );
    fs::write(&path, plist)?;
    let uid = Command::new("id").arg("-u").output()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    let domain = format!("gui/{uid}");
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/dev.pastebridge.daemon")])
        .status();
    let load = Command::new("launchctl")
        .args(["bootstrap", &domain, &path.display().to_string()])
        .status()?;
    let _ = Command::new("launchctl")
        .args(["enable", &format!("{domain}/dev.pastebridge.daemon")])
        .status();
    if load.success() {
        println!("Installed LaunchAgent {}", path.display());
        println!("Pastebridge will start when you log in.");
    } else {
        println!("Wrote {} — load it with:", path.display());
        println!("  launchctl bootstrap gui/$UID {}", path.display());
    }
    Ok(())
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
    println!("Removed the Pastebridge LaunchAgent.");
    Ok(())
}
