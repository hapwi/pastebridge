use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, info, warn};

use crate::clipboard::Clipboard;
use crate::config::{parse_addr, Config, Paths};
use crate::crypto::device_id_from_cert;
use crate::discovery::{self, SERVICE};
use crate::identity::{Identity, PeerList};
use crate::sync::{self, IncomingClip, OutgoingClip};
use crate::tls::{self, peer_cert, PinStore};

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub running: bool,
    pub pid: u32,
    pub device_id: String,
    pub name: String,
    pub port: u16,
    pub backend: String,
    pub peers: Vec<PeerStatus>,
    pub last_sent: Option<String>,
    pub last_received: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeerStatus {
    pub device_id: String,
    pub name: String,
    pub connected: bool,
    pub last_addr: Option<String>,
}

pub async fn run(cfg: Config, paths: Paths, identity: Identity) -> Result<()> {
    write_pid(&paths)?;
    let _pid_guard = PidGuard {
        path: paths.pid_file.clone(),
    };

    let peers = PeerList::load(&paths)?;
    if peers.peers.is_empty() {
        warn!("no paired devices — run `pastebridge pair` on both computers");
    } else {
        info!(
            "loaded {} paired device{}",
            peers.peers.len(),
            if peers.peers.len() == 1 { "" } else { "s" }
        );
    }

    let pins = PinStore::new(peers.pins()?);
    let server = tls::server_config(&identity, pins.clone(), true)?;
    let bind: SocketAddr = format!("0.0.0.0:{}", cfg.port).parse()?;
    let endpoint = quinn::Endpoint::server(server, bind)?;
    let client_cfg = tls::client_config(&identity, pins.clone(), false)?;
    let mut client_endpoint = endpoint.clone();
    client_endpoint.set_default_client_config(client_cfg);

    info!(
        "listening on {bind} as {} ({})",
        identity.name, identity.device_id
    );

    let mdns = match discovery::start_mdns() {
        Ok(mdns) => {
            if let Err(err) = discovery::advertise(
                &mdns,
                SERVICE,
                &identity.device_id,
                &identity.name,
                cfg.port,
            ) {
                warn!("mDNS advertise failed: {err}");
            }
            Some(mdns)
        }
        Err(err) => {
            warn!("mDNS unavailable: {err}");
            None
        }
    };

    let mut clipboard = Clipboard::open()?;
    info!("clipboard backend: {}", clipboard.backend);

    let (out_tx, _) = broadcast::channel::<OutgoingClip>(16);
    let (in_tx, mut in_rx) = mpsc::channel::<IncomingClip>(16);
    let connected: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let last_sent = Arc::new(Mutex::new(None::<String>));
    let last_recv = Arc::new(Mutex::new(None::<String>));
    let last_err = Arc::new(Mutex::new(None::<String>));
    let peers = Arc::new(Mutex::new(peers));

    // Incoming QUIC
    {
        let endpoint = endpoint.clone();
        let identity = identity.clone();
        let out_tx = out_tx.clone();
        let in_tx = in_tx.clone();
        let connected = connected.clone();
        let pins = pins.clone();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                match incoming.await {
                    Ok(conn) => {
                        let Ok(cert) = peer_cert(&conn) else {
                            warn!("rejected connection without certificate");
                            continue;
                        };
                        if !pins.contains(cert.as_ref()) {
                            warn!(
                                "rejected unpinned connection from {}",
                                conn.remote_address()
                            );
                            continue;
                        }
                        let peer_id = device_id_from_cert(cert.as_ref());
                        let mut guard = connected.lock().await;
                        if !guard.insert(peer_id.clone()) {
                            continue;
                        }
                        drop(guard);
                        let identity = identity.clone();
                        let outgoing = out_tx.subscribe();
                        let incoming = in_tx.clone();
                        let connected = connected.clone();
                        tokio::spawn(async move {
                            let _ =
                                sync::run_session(conn, false, identity, outgoing, incoming).await;
                            connected.lock().await.remove(&peer_id);
                        });
                    }
                    Err(err) => warn!("incoming handshake failed: {err}"),
                }
            }
        });
    }

    // mDNS browse → connect to known peers with higher device id
    if let Some(mdns) = mdns.as_ref() {
        if let Ok(mut browse) = discovery::browse(mdns, SERVICE) {
            let our_id = identity.device_id.clone();
            let client_endpoint = client_endpoint.clone();
            let identity = identity.clone();
            let out_tx = out_tx.clone();
            let in_tx = in_tx.clone();
            let connected = connected.clone();
            let peers = peers.clone();
            tokio::spawn(async move {
                while let Some(found) = browse.recv().await {
                    if found.device_id == our_id {
                        continue;
                    }
                    let known = {
                        let list = peers.lock().await;
                        list.get(&found.device_id).cloned()
                    };
                    let Some(_) = known else { continue };
                    if found.device_id < our_id {
                        continue;
                    }
                    if connected.lock().await.contains(&found.device_id) {
                        continue;
                    }
                    try_connect(
                        &client_endpoint,
                        found.addr,
                        &found.device_id,
                        &identity,
                        &out_tx,
                        &in_tx,
                        &connected,
                    )
                    .await;
                }
            });
        }
    }

    // Periodic reconnect using last_addr + static_peers
    {
        let client_endpoint = client_endpoint.clone();
        let identity = identity.clone();
        let out_tx = out_tx.clone();
        let in_tx = in_tx.clone();
        let connected = connected.clone();
        let peers = peers.clone();
        let our_id = identity.device_id.clone();
        let static_peers = cfg.static_peers.clone();
        let port = cfg.port;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                let snapshot = peers.lock().await.peers.clone();
                for peer in snapshot {
                    if connected.lock().await.contains(&peer.device_id) {
                        continue;
                    }
                    let should_dial = peer.device_id > our_id;
                    if should_dial {
                        if let Some(addr) = peer.last_addr.as_deref() {
                            if let Ok(addr) = parse_addr(addr, port) {
                                try_connect(
                                    &client_endpoint,
                                    addr,
                                    &peer.device_id,
                                    &identity,
                                    &out_tx,
                                    &in_tx,
                                    &connected,
                                )
                                .await;
                            }
                        }
                    }
                }
                for spec in &static_peers {
                    if let Ok(addr) = parse_addr(spec, port) {
                        try_connect(
                            &client_endpoint,
                            addr,
                            spec,
                            &identity,
                            &out_tx,
                            &in_tx,
                            &connected,
                        )
                        .await;
                    }
                }
            }
        });
    }

    // Reload peers.json (after pairing while daemon is running)
    {
        let paths = paths.clone();
        let peers = peers.clone();
        let pins = pins.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            let mut mtime = std::fs::metadata(&paths.peers_file)
                .and_then(|m| m.modified())
                .ok();
            loop {
                tick.tick().await;
                let new_mtime = std::fs::metadata(&paths.peers_file)
                    .and_then(|m| m.modified())
                    .ok();
                if new_mtime != mtime {
                    mtime = new_mtime;
                    if let Ok(list) = PeerList::load(&paths) {
                        if let Ok(new_pins) = list.pins() {
                            pins.replace(new_pins);
                        }
                        info!("reloaded {} peer(s)", list.peers.len());
                        *peers.lock().await = list;
                    }
                }
            }
        });
    }

    // Clipboard poller lives on a real thread: NSPasteboard / Wayland
    // types are often !Send.
    {
        let out_tx = out_tx.clone();
        let identity = identity.clone();
        let last_sent = last_sent.clone();
        let interval = Duration::from_millis(cfg.poll_interval_ms.max(150));
        let max_bytes = cfg.max_payload_bytes;
        let sync_images = cfg.sync_images;
        std::thread::Builder::new()
            .name("clipboard-poll".into())
            .spawn(move || loop {
                if let Some(clip) = clipboard.poll() {
                    if clip.concealed {
                        debug!("concealed clipboard — not syncing");
                        continue;
                    }
                    if clip.mime.starts_with("image/") && !sync_images {
                        continue;
                    }
                    if clip.bytes.len() > max_bytes {
                        warn!(
                            "skipping clipboard item of {} bytes (limit {max_bytes})",
                            clip.bytes.len()
                        );
                        continue;
                    }
                    let hash = clip.hash();
                    let outgoing = OutgoingClip {
                        origin: identity.device_id.clone(),
                        mime: clip.mime,
                        hash,
                        bytes: clip.bytes,
                    };
                    *last_sent.blocking_lock() = Some(now());
                    let _ = out_tx.send(outgoing);
                }
                std::thread::sleep(interval);
            })
            .context("starting clipboard thread")?;
    }

    // Apply remote clips
    {
        let last_recv = last_recv.clone();
        let last_err = last_err.clone();
        std::thread::Builder::new()
            .name("clipboard-apply".into())
            .spawn(move || {
                let mut clipboard = match Clipboard::open() {
                    Ok(cb) => cb,
                    Err(err) => {
                        *last_err.blocking_lock() = Some(err.to_string());
                        return;
                    }
                };
                while let Some(clip) = in_rx.blocking_recv() {
                    let applied = crate::clipboard::Clip {
                        mime: clip.mime,
                        bytes: clip.bytes,
                        concealed: false,
                    };
                    match clipboard.set(&applied) {
                        Ok(()) => {
                            *last_recv.blocking_lock() = Some(now());
                            info!("clipboard from {}", clip.from);
                        }
                        Err(err) => {
                            warn!("failed to set clipboard: {err}");
                            *last_err.blocking_lock() = Some(err.to_string());
                        }
                    }
                }
            })
            .context("starting clipboard apply thread")?;
    }

    // Status file
    {
        let paths = paths.clone();
        let identity = identity.clone();
        let peers = peers.clone();
        let connected = connected.clone();
        let last_sent = last_sent.clone();
        let last_recv = last_recv.clone();
        let last_err = last_err.clone();
        let backend = {
            Clipboard::open()
                .map(|c| c.backend)
                .unwrap_or_else(|_| "unknown".into())
        };
        let port = cfg.port;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            loop {
                tick.tick().await;
                let connected_set = connected.lock().await.clone();
                let peer_list = peers.lock().await.clone();
                let status = Status {
                    running: true,
                    pid: std::process::id(),
                    device_id: identity.device_id.clone(),
                    name: identity.name.clone(),
                    port,
                    backend: backend.clone(),
                    peers: peer_list
                        .peers
                        .iter()
                        .map(|p| PeerStatus {
                            connected: connected_set.contains(&p.device_id),
                            device_id: p.device_id.clone(),
                            name: p.name.clone(),
                            last_addr: p.last_addr.clone(),
                        })
                        .collect(),
                    last_sent: last_sent.lock().await.clone(),
                    last_received: last_recv.lock().await.clone(),
                    last_error: last_err.lock().await.clone(),
                };
                if let Ok(json) = serde_json::to_string_pretty(&status) {
                    let _ = crate::config::write_secret_file(&paths.status_file, &json);
                }
            }
        });
    }

    info!("pastebridge is running — copy on one computer, paste on the other");
    tokio::signal::ctrl_c().await.ok();
    info!("shutting down");
    endpoint.close(0u32.into(), b"bye");
    Ok(())
}

async fn try_connect(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    peer_key: &str,
    identity: &Identity,
    out_tx: &broadcast::Sender<OutgoingClip>,
    in_tx: &mpsc::Sender<IncomingClip>,
    connected: &Arc<Mutex<HashSet<String>>>,
) {
    if connected.lock().await.contains(peer_key) {
        return;
    }
    match endpoint.connect(addr, "pastebridge") {
        Ok(connecting) => match connecting.await {
            Ok(conn) => {
                let peer_id = peer_cert(&conn)
                    .map(|c| device_id_from_cert(c.as_ref()))
                    .unwrap_or_else(|_| peer_key.to_string());
                {
                    let mut guard = connected.lock().await;
                    if !guard.insert(peer_id.clone()) {
                        return;
                    }
                }
                let identity = identity.clone();
                let outgoing = out_tx.subscribe();
                let incoming = in_tx.clone();
                let connected = connected.clone();
                tokio::spawn(async move {
                    let _ = sync::run_session(conn, true, identity, outgoing, incoming).await;
                    connected.lock().await.remove(&peer_id);
                });
            }
            Err(err) => tracing::debug!("connect {addr} failed: {err}"),
        },
        Err(err) => tracing::debug!("dial {addr} failed: {err}"),
    }
}

fn write_pid(paths: &Paths) -> Result<()> {
    if let Some(pid) = running_pid(paths) {
        anyhow::bail!("pastebridge is already running (pid {pid})");
    }
    std::fs::write(&paths.pid_file, std::process::id().to_string())
        .with_context(|| format!("writing {}", paths.pid_file.display()))?;
    Ok(())
}

pub fn running_pid(paths: &Paths) -> Option<u32> {
    let text = std::fs::read_to_string(&paths.pid_file).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    if process_is_pastebridge(pid) {
        Some(pid)
    } else {
        None
    }
}

fn process_is_pastebridge(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let path = format!("/proc/{pid}/comm");
        std::fs::read_to_string(path)
            .map(|s| s.contains("pastebridge"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("pastebridge"))
            .unwrap_or(false)
    }
}

fn now() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

struct PidGuard {
    path: std::path::PathBuf,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
