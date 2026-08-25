use anyhow::Result;
use std::net::UdpSocket;

use crate::clipboard::Clipboard;
use crate::config::Paths;
use crate::daemon;
use crate::discovery;
use crate::identity::{Identity, PeerList};
use crate::Config;

pub fn run(cfg: &Config, paths: &Paths, identity: &Identity) -> Result<()> {
    let mut failed = 0;

    println!();
    println!("  {}", identity.name);

    if !paths.config_dir.exists() {
        fail(
            "config",
            &paths.config_dir.display().to_string(),
            &mut failed,
        );
    }

    let peers = PeerList::load(paths)?;
    if peers.peers.is_empty() {
        println!("  paired     none");
    } else {
        let names: Vec<_> = peers.peers.iter().map(|p| p.name.as_str()).collect();
        println!("  paired     {}", names.join(", "));
    }

    match Clipboard::open() {
        Ok(mut cb) => match cb.read() {
            Some(clip) => println!(
                "  clipboard  {}  {}  {} B",
                cb.backend,
                clip.mime,
                clip.bytes.len()
            ),
            None => println!("  clipboard  {}  empty", cb.backend),
        },
        Err(err) => fail("clipboard", &err.to_string(), &mut failed),
    }

    let daemon_pid = daemon::running_pid(paths);
    let port_ok = UdpSocket::bind(("0.0.0.0", cfg.port)).is_ok() || daemon_pid.is_some();
    if !port_ok {
        fail("port", &format!("{} in use", cfg.port), &mut failed);
    }

    if std::env::var_os("WAYLAND_DISPLAY").is_some() && !(which("wl-copy") && which("wl-paste")) {
        fail(
            "wl-clipboard",
            "install with  sudo dnf install wl-clipboard",
            &mut failed,
        );
    }

    match discovery::tailscale_network() {
        Ok(Some(network)) => {
            let n = network.pairable_peers().count();
            println!(
                "  tailscale  {}  {} peer{}",
                network.local_ip,
                n,
                if n == 1 { "" } else { "s" }
            );
        }
        Ok(None) | Err(_) => {}
    }

    match daemon_pid {
        Some(pid) => println!("  daemon     running  pid {pid}"),
        None => println!("  daemon     stopped"),
    }

    println!();
    if failed > 0 {
        println!("  {failed} need attention");
        println!();
    }
    Ok(())
}

fn fail(label: &str, detail: &str, failed: &mut i32) {
    println!("! {label:<10} {detail}");
    *failed += 1;
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(name).is_file()))
}
