#![cfg(target_os = "linux")]

use pastebridge::config::Paths;
use pastebridge::identity::{Peer, PeerList};
use pastebridge::{Config, Identity};
use std::fs;
use std::net::UdpSocket;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn paths(dir: &Path) -> Paths {
    Paths {
        config_dir: dir.into(),
        config_file: dir.join("config.toml"),
        identity_file: dir.join("identity.json"),
        peers_file: dir.join("peers.json"),
        status_file: dir.join("status.json"),
        pid_file: dir.join("pastebridge.pid"),
    }
}

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn configure(paths: &Paths, identity: &Identity, other: &Identity, port: u16, other_port: u16) {
    fs::create_dir_all(&paths.config_dir).unwrap();
    let config = Config {
        bind_address: "127.0.0.1".parse().unwrap(),
        port,
        poll_interval_ms: 150,
        static_peers: vec![format!("127.0.0.1:{other_port}")],
        ..Config::default()
    };
    fs::write(&paths.config_file, toml::to_string(&config).unwrap()).unwrap();
    identity.save(&paths.identity_file).unwrap();
    PeerList {
        peers: vec![Peer {
            device_id: other.device_id.clone(),
            name: other.name.clone(),
            cert_pem: other.cert_pem.clone(),
            cert_sha256: other.fingerprint().unwrap(),
            last_addr: Some(format!("127.0.0.1:{other_port}")),
            paired_at: "2026-01-01T00:00:00Z".into(),
        }],
    }
    .save(paths)
    .unwrap();
}

fn start(paths: &Paths, tools: &Path) -> Daemon {
    Daemon(
        Command::new(env!("CARGO_BIN_EXE_pastebridge"))
            .arg("start")
            .env("PASTEBRIDGE_HOME", &paths.config_dir)
            .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
            .env("DISPLAY", ":99999")
            .env("XDG_RUNTIME_DIR", &paths.config_dir)
            .env("FIXTURE_CLIPBOARD", paths.config_dir.join("clipboard"))
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(paths.config_dir.join("daemon.log")).unwrap())
            .spawn()
            .unwrap(),
    )
}

fn wait_for(
    description: &str,
    timeout: Duration,
    mut check: impl FnMut() -> bool,
    homes: &[&Paths],
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let logs: Vec<_> = homes
        .iter()
        .map(|home| fs::read_to_string(home.config_dir.join("daemon.log")).unwrap_or_default())
        .collect();
    panic!("timed out waiting for {description}: {logs:?}");
}

fn connected(paths: &Paths) -> bool {
    pastebridge::service::ready_status(paths, env!("CARGO_PKG_VERSION"))
        .is_some_and(|status| status.peers.first().is_some_and(|peer| peer.connected))
}

#[test]
fn daemon_connects_to_a_late_peer_and_recovers_after_its_restart() {
    pastebridge::init_crypto();
    let temp = tempfile::tempdir().unwrap();
    let tools = temp.path().join("bin");
    fs::create_dir(&tools).unwrap();
    let xclip = tools.join("xclip");
    fs::write(
        &xclip,
        r#"#!/bin/sh
case " $* " in
  *" TARGETS "*) printf 'UTF8_STRING\n'; exit 0 ;;
  *" image/"*|*" text/uri-list "*) exit 1 ;;
  *" -o "*) cat "$FIXTURE_CLIPBOARD"; exit 0 ;;
  *) cat > "$FIXTURE_CLIPBOARD" ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&xclip, fs::Permissions::from_mode(0o755)).unwrap();
    let tailscale = tools.join("tailscale");
    fs::write(&tailscale, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(tailscale, fs::Permissions::from_mode(0o755)).unwrap();
    let mut first = Identity::generate("first").unwrap();
    let mut second = Identity::generate("second").unwrap();
    if first.device_id > second.device_id {
        std::mem::swap(&mut first, &mut second);
    }
    let a = paths(&temp.path().join("a"));
    let b = paths(&temp.path().join("b"));
    let port_a = free_port();
    let mut port_b = free_port();
    while port_b == port_a {
        port_b = free_port();
    }
    configure(&a, &first, &second, port_a, port_b);
    configure(&b, &second, &first, port_b, port_a);
    fs::write(a.config_dir.join("clipboard"), "initial A").unwrap();
    fs::write(b.config_dir.join("clipboard"), "initial B").unwrap();
    let _daemon_a = start(&a, &tools);
    pastebridge::service::wait_for_ready(&a, env!("CARGO_PKG_VERSION"), Duration::from_secs(5))
        .unwrap();
    assert!(!connected(&a));
    let daemon_b = start(&b, &tools);
    wait_for(
        "late peer connection",
        Duration::from_secs(20),
        || connected(&a) && connected(&b),
        &[&a, &b],
    );
    fs::write(a.config_dir.join("clipboard"), "copy after peer appeared").unwrap();
    wait_for(
        "first clipboard delivery",
        Duration::from_secs(5),
        || {
            fs::read_to_string(b.config_dir.join("clipboard"))
                .ok()
                .as_deref()
                == Some("copy after peer appeared")
        },
        &[&a, &b],
    );
    drop(daemon_b);
    let _restarted_b = start(&b, &tools);
    wait_for(
        "peer restart recovery",
        Duration::from_secs(45),
        || connected(&a) && connected(&b),
        &[&a, &b],
    );
    fs::write(b.config_dir.join("clipboard"), "copy after restart").unwrap();
    wait_for(
        "clipboard delivery after restart",
        Duration::from_secs(5),
        || {
            fs::read_to_string(a.config_dir.join("clipboard"))
                .ok()
                .as_deref()
                == Some("copy after restart")
        },
        &[&a, &b],
    );
}
