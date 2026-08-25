use anyhow::Result;
use std::net::TcpListener;

use crate::clipboard::Clipboard;
use crate::config::Paths;
use crate::daemon;
use crate::identity::{Identity, PeerList};
use crate::Config;

pub fn run(cfg: &Config, paths: &Paths, identity: &Identity) -> Result<()> {
    println!("Pastebridge doctor");
    println!("==================");
    println!();

    let mut failed = 0;

    check("config directory", paths.config_dir.exists(), &mut failed);
    println!("  {}", paths.config_dir.display());

    check("identity", paths.identity_file.exists(), &mut failed);
    println!("  device {} ({})", identity.name, identity.device_id);

    let peers = PeerList::load(paths)?;
    check("paired devices", !peers.peers.is_empty(), &mut failed);
    if peers.peers.is_empty() {
        println!("  none yet — run `pastebridge pair` on both computers");
    } else {
        for peer in &peers.peers {
            println!("  {} ({})", peer.name, peer.device_id);
        }
    }

    match Clipboard::open() {
        Ok(mut cb) => {
            check(&format!("clipboard ({})", cb.backend), true, &mut failed);
            match cb.read() {
                Some(clip) => {
                    println!("  current item: {} ({} bytes)", clip.mime, clip.bytes.len())
                }
                None => println!("  clipboard is empty (that is ok)"),
            }
        }
        Err(err) => {
            check(&format!("clipboard ({err})"), false, &mut failed);
        }
    }

    let port_ok = TcpListener::bind(("0.0.0.0", cfg.port)).is_ok();
    check(
        &format!("udp/tcp port {} free for setup", cfg.port),
        port_ok || daemon::running_pid(paths).is_some(),
        &mut failed,
    );

    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let wl = which("wl-copy") && which("wl-paste");
        check("wl-clipboard (Wayland)", wl, &mut failed);
        if !wl {
            println!("  install it:  sudo dnf install wl-clipboard");
            println!("               sudo apt install wl-clipboard");
        }
        if gnome_wayland() {
            println!();
            println!("  Note: GNOME Wayland has limited clipboard APIs.");
            println!("  If paste from the other computer is unreliable,");
            println!("  install wl-clipboard and keep a terminal session logged in.");
        }
    }

    if let Some(pid) = daemon::running_pid(paths) {
        println!();
        println!("  daemon is running (pid {pid})");
    } else {
        println!();
        println!("  daemon is not running — start it with `pastebridge start`");
        println!("  or `pastebridge install-service`");
    }

    println!();
    if failed == 0 {
        println!("All checks passed.");
    } else {
        println!("{failed} check(s) need attention.");
    }
    Ok(())
}

fn check(label: &str, ok: bool, failed: &mut i32) {
    if ok {
        println!("[ok]  {label}");
    } else {
        println!("[!!]  {label}");
        *failed += 1;
    }
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
}

fn gnome_wayland() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.to_ascii_uppercase().contains("GNOME"))
        .unwrap_or(false)
        && std::env::var_os("WAYLAND_DISPLAY").is_some()
}
