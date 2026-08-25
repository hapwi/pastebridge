//! Two processes pair over localhost using --yes and --connect.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    env!("CARGO_BIN_EXE_pastebridge").into()
}

#[test]
fn pair_over_localhost() {
    let tmp = tempfile::tempdir().unwrap();
    let home_a = tmp.path().join("a");
    let home_b = tmp.path().join("b");
    fs::create_dir_all(&home_a).unwrap();
    fs::create_dir_all(&home_b).unwrap();
    fs::write(
        home_a.join("config.toml"),
        "pair_port = 27421\nport = 27422\n",
    )
    .unwrap();
    fs::write(
        home_b.join("config.toml"),
        "pair_port = 27423\nport = 27424\n",
    )
    .unwrap();

    let mut server = Command::new(bin())
        .args(["pair", "--yes"])
        .env("PASTEBRIDGE_HOME", &home_a)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn server");

    thread::sleep(Duration::from_millis(400));

    let client = Command::new(bin())
        .args(["pair", "--yes", "--connect", "127.0.0.1:27421"])
        .env("PASTEBRIDGE_HOME", &home_b)
        .env("RUST_LOG", "warn")
        .output()
        .expect("run client");

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if server.try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = server.kill();
    let server_out = server.wait_with_output().unwrap();

    let client_stdout = String::from_utf8_lossy(&client.stdout);
    let client_stderr = String::from_utf8_lossy(&client.stderr);
    let server_stdout = String::from_utf8_lossy(&server_out.stdout);
    let server_stderr = String::from_utf8_lossy(&server_out.stderr);

    assert!(
        client.status.success(),
        "client failed:\n{client_stdout}\n{client_stderr}"
    );
    assert!(
        server_out.status.success(),
        "server failed:\n{server_stdout}\n{server_stderr}"
    );

    let peers_a = fs::read_to_string(home_a.join("peers.json")).unwrap();
    let peers_b = fs::read_to_string(home_b.join("peers.json")).unwrap();
    assert!(peers_a.contains("device_id"), "{peers_a}");
    assert!(peers_b.contains("device_id"), "{peers_b}");
}
