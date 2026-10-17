use std::fs;
use std::process::{Command, Output};

use pastebridge::identity::{Identity, Peer, PeerList};

fn fixture() -> (tempfile::TempDir, PeerList) {
    pastebridge::init_crypto();
    let home = tempfile::tempdir().unwrap();
    let peers = PeerList {
        peers: ["Office Mac", "Office Mac"]
            .into_iter()
            .map(|name| {
                let identity = Identity::generate(name).unwrap();
                Peer {
                    device_id: identity.device_id.clone(),
                    name: name.into(),
                    cert_sha256: identity.fingerprint().unwrap(),
                    cert_pem: identity.cert_pem,
                    last_addr: Some("127.0.0.1:27419".into()),
                    paired_at: "2026-01-01T00:00:00Z".into(),
                }
            })
            .collect(),
    };
    fs::write(
        home.path().join("peers.json"),
        serde_json::to_vec(&peers).unwrap(),
    )
    .unwrap();
    (home, peers)
}

fn run(home: &tempfile::TempDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pastebridge"))
        .args(args)
        .env("PASTEBRIDGE_HOME", home.path())
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

#[test]
fn device_listing_is_scriptable_without_exposing_certificates() {
    let (home, peers) = fixture();
    let result = run(&home, &["devices", "--json"]);
    assert!(result.status.success());
    let devices: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(devices.as_array().unwrap().len(), 2);
    assert_eq!(devices[0]["device_id"], peers.peers[0].device_id);
    assert_eq!(devices[0]["connected"], false);
    assert!(!String::from_utf8_lossy(&result.stdout).contains("CERTIFICATE"));
    let ids = run(&home, &["list", "--quiet"]);
    assert_eq!(
        String::from_utf8(ids.stdout).unwrap(),
        format!(
            "{}\n{}\n",
            peers.peers[0].device_id, peers.peers[1].device_id
        )
    );
    let dashboard = run(&home, &[]);
    assert!(dashboard.status.success());
    let text = String::from_utf8(dashboard.stdout).unwrap();
    assert!(text.contains("Service stopped"));
    assert!(text.contains("Office Mac"));
    assert!(text.contains("pastebridge unpair"));
    assert!(!text.contains('\x1b'));
}

#[test]
fn unpair_requires_an_unambiguous_device_and_confirmation() {
    let (home, peers) = fixture();
    let path = home.path().join("peers.json");
    let original = fs::read(&path).unwrap();
    let ambiguous = run(&home, &["unpair", "Office Mac", "--yes"]);
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("more than one computer"));
    assert_eq!(fs::read(&path).unwrap(), original);
    let unconfirmed = run(&home, &["unpair", &peers.peers[0].device_id]);
    assert!(!unconfirmed.status.success());
    assert_eq!(fs::read(&path).unwrap(), original);
    let confirmed = run(&home, &["unpair", &peers.peers[0].device_id[..8], "--yes"]);
    assert!(confirmed.status.success());
    let remaining: PeerList = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(remaining.peers.len(), 1);
    assert_eq!(remaining.peers[0].device_id, peers.peers[1].device_id);
}
