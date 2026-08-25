use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use tokio::sync::{broadcast, mpsc, Mutex, Semaphore};
use tracing::{debug, info, warn};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::clipboard::Clipboard;
use crate::config::{parse_addr, Config, Paths};
use crate::crypto::device_id_from_cert;
use crate::discovery::{self, SERVICE};
use crate::identity::{Identity, PeerList};
use crate::sync::{self, IncomingClip, OutgoingClip};
use crate::tls::{self, peer_cert, PinStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    pub device_id: String,
    pub name: String,
    pub connected: bool,
    pub last_addr: Option<String>,
}

#[derive(Clone)]
struct SessionConnector {
    endpoint: quinn::Endpoint,
    identity: Identity,
    out_tx: broadcast::Sender<OutgoingClip>,
    in_tx: mpsc::Sender<IncomingClip>,
    connected: Arc<Mutex<HashMap<String, quinn::Connection>>>,
    max_payload_bytes: usize,
}

pub async fn run(cfg: Config, paths: Paths, identity: Identity) -> Result<()> {
    crate::clipboard::wait_for_display();
    let pid_file = lock_pid(&paths)?;
    let _pid_guard = PidGuard {
        _file: pid_file,
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
    let bind = SocketAddr::from((cfg.bind_address, cfg.port));
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
    let connected: Arc<Mutex<HashMap<String, quinn::Connection>>> =
        Arc::new(Mutex::new(HashMap::new()));
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
        let max_payload_bytes = cfg.max_payload_bytes;
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
                        if identity.device_id < peer_id {
                            conn.close(0u32.into(), b"peer uses the client connection");
                            continue;
                        }
                        let mut guard = connected.lock().await;
                        if guard.contains_key(&peer_id) {
                            conn.close(0u32.into(), b"duplicate connection");
                            continue;
                        }
                        guard.insert(peer_id.clone(), conn.clone());
                        drop(guard);
                        let identity = identity.clone();
                        let outgoing = out_tx.subscribe();
                        let incoming = in_tx.clone();
                        let connected = connected.clone();
                        tokio::spawn(async move {
                            let _ = sync::run_session(
                                conn,
                                false,
                                identity,
                                outgoing,
                                incoming,
                                max_payload_bytes,
                            )
                            .await;
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
            let connector = SessionConnector {
                endpoint: client_endpoint.clone(),
                identity: identity.clone(),
                out_tx: out_tx.clone(),
                in_tx: in_tx.clone(),
                connected: connected.clone(),
                max_payload_bytes: cfg.max_payload_bytes,
            };
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
                    if connector
                        .connected
                        .lock()
                        .await
                        .contains_key(&found.device_id)
                    {
                        continue;
                    }
                    try_connect(&connector, found.addr, &found.device_id).await;
                }
            });
        }
    }

    // Periodic reconnect using saved addresses, static peers, and Tailscale.
    {
        let connector = SessionConnector {
            endpoint: client_endpoint.clone(),
            identity: identity.clone(),
            out_tx: out_tx.clone(),
            in_tx: in_tx.clone(),
            connected: connected.clone(),
            max_payload_bytes: cfg.max_payload_bytes,
        };
        let peers = peers.clone();
        let our_id = identity.device_id.clone();
        let static_peers = cfg.static_peers.clone();
        let port = cfg.port;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                let snapshot = peers.lock().await.peers.clone();
                for peer in snapshot {
                    if connector
                        .connected
                        .lock()
                        .await
                        .contains_key(&peer.device_id)
                    {
                        continue;
                    }
                    let should_dial = peer.device_id > our_id;
                    if should_dial {
                        if let Some(addr) = peer.last_addr.as_deref() {
                            if let Ok(addr) = parse_addr(addr, port) {
                                try_connect(&connector, addr, &peer.device_id).await;
                            }
                        }
                    }
                }
                for spec in &static_peers {
                    if let Ok(addr) = parse_addr(spec, port) {
                        try_connect(&connector, addr, spec).await;
                    }
                }
                if connector.connected.lock().await.len() < peers.lock().await.peers.len() {
                    let tailscale = tokio::task::spawn_blocking(discovery::tailscale_network).await;
                    if let Ok(Ok(Some(network))) = tailscale {
                        let limit = Arc::new(Semaphore::new(16));
                        let mut attempts = tokio::task::JoinSet::new();
                        for peer in network.pairable_peers() {
                            let addr = SocketAddr::from((peer.ip, port));
                            let connector = connector.clone();
                            let limit = limit.clone();
                            attempts.spawn(async move {
                                let Ok(_permit) = limit.acquire_owned().await else {
                                    return;
                                };
                                try_connect(&connector, addr, &addr.to_string()).await;
                            });
                        }
                        while let Some(result) = attempts.join_next().await {
                            if let Err(err) = result {
                                debug!("Tailscale connection task failed: {err}");
                            }
                        }
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
        let connected = connected.clone();
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
                        connected.lock().await.retain(|peer_id, connection| {
                            let keep = list.get(peer_id).is_some();
                            if !keep {
                                connection.close(0u32.into(), b"peer unpaired");
                            }
                            keep
                        });
                        info!("reloaded {} peer(s)", list.peers.len());
                        *peers.lock().await = list;
                    }
                }
            }
        });
    }

    // Bridge the async network receiver to the clipboard's owning thread.
    let (clipboard_in_tx, clipboard_in_rx) = std_mpsc::channel::<IncomingClip>();
    tokio::spawn(async move {
        while let Some(clip) = in_rx.recv().await {
            if clipboard_in_tx.send(clip).is_err() {
                break;
            }
        }
    });

    // Poll, apply, and expire on one real thread: NSPasteboard / Wayland types
    // are often !Send, and one owner keeps remote echo suppression effective.
    {
        let out_tx = out_tx.clone();
        let identity = identity.clone();
        let last_sent = last_sent.clone();
        let last_recv = last_recv.clone();
        let last_err = last_err.clone();
        let interval = Duration::from_millis(cfg.poll_interval_ms.max(150));
        let max_bytes = cfg.max_payload_bytes;
        let sync_images = cfg.sync_images;
        let clipboard_ttl = (cfg.clipboard_ttl_seconds != 0)
            .then(|| Duration::from_secs(cfg.clipboard_ttl_seconds));
        std::thread::Builder::new()
            .name("clipboard-poll".into())
            .spawn(move || {
                let mut next_poll = Instant::now();
                let mut pending_expiry = None::<(crate::clipboard::Clip, Instant)>;
                loop {
                    let current_time = Instant::now();
                    if current_time >= next_poll {
                        if let Some(clip) = clipboard.poll() {
                            if clip.concealed {
                                debug!("concealed clipboard — not syncing");
                            } else if clip.mime.starts_with("image/") && !sync_images {
                                debug!("image clipboard — image syncing disabled");
                            } else if clip.bytes.len() > max_bytes {
                                warn!(
                                    "skipping clipboard item of {} bytes (limit {max_bytes})",
                                    clip.bytes.len()
                                );
                            } else {
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
                        }
                        next_poll = current_time + interval;
                    }

                    if pending_expiry
                        .as_ref()
                        .is_some_and(|(_, deadline)| Instant::now() >= *deadline)
                    {
                        let (expected, _) = pending_expiry
                            .take()
                            .expect("pending clipboard expiry disappeared");
                        match clipboard.clear_if_matches(&expected) {
                            Ok(true) => info!("expired unchanged remote clipboard"),
                            Ok(false) => {
                                debug!("remote clipboard changed before expiry — keeping it")
                            }
                            Err(err) => {
                                warn!("failed to expire clipboard: {err}");
                                *last_err.blocking_lock() = Some(err.to_string());
                            }
                        }
                    }

                    let wake_at = pending_expiry
                        .as_ref()
                        .map(|(_, deadline)| next_poll.min(*deadline))
                        .unwrap_or(next_poll);
                    let timeout = wake_at.saturating_duration_since(Instant::now());
                    let incoming = match clipboard_in_rx.recv_timeout(timeout) {
                        Ok(clip) => clip,
                        Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    let applied = crate::clipboard::Clip {
                        mime: incoming.mime,
                        bytes: incoming.bytes,
                        concealed: false,
                    };
                    match clipboard.set(&applied) {
                        Ok(()) => {
                            *last_recv.blocking_lock() = Some(now());
                            info!("clipboard from {}", incoming.from);
                            pending_expiry =
                                clipboard_ttl.map(|ttl| (applied, Instant::now() + ttl));
                        }
                        Err(err) => {
                            warn!("failed to set clipboard: {err}");
                            *last_err.blocking_lock() = Some(err.to_string());
                        }
                    }
                }
            })
            .context("starting clipboard thread")?;
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
                            connected: connected_set.contains_key(&p.device_id),
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

async fn try_connect(connector: &SessionConnector, addr: SocketAddr, peer_key: &str) {
    if connector.connected.lock().await.contains_key(peer_key) {
        return;
    }
    match connector.endpoint.connect(addr, "pastebridge") {
        Ok(connecting) => match tokio::time::timeout(Duration::from_secs(8), connecting).await {
            Ok(Ok(conn)) => {
                let Ok(cert) = peer_cert(&conn) else {
                    conn.close(0u32.into(), b"peer did not present a certificate");
                    return;
                };
                let peer_id = device_id_from_cert(cert.as_ref());
                if connector.identity.device_id >= peer_id {
                    conn.close(0u32.into(), b"peer uses the client connection");
                    return;
                }
                {
                    let mut guard = connector.connected.lock().await;
                    if guard.contains_key(&peer_id) {
                        conn.close(0u32.into(), b"duplicate connection");
                        return;
                    }
                    guard.insert(peer_id.clone(), conn.clone());
                }
                let identity = connector.identity.clone();
                let outgoing = connector.out_tx.subscribe();
                let incoming = connector.in_tx.clone();
                let connected = connector.connected.clone();
                let max_payload_bytes = connector.max_payload_bytes;
                tokio::spawn(async move {
                    let _ = sync::run_session(
                        conn,
                        true,
                        identity,
                        outgoing,
                        incoming,
                        max_payload_bytes,
                    )
                    .await;
                    connected.lock().await.remove(&peer_id);
                });
            }
            Ok(Err(err)) => tracing::debug!("connect {addr} failed: {err}"),
            Err(_) => tracing::debug!("connect {addr} timed out"),
        },
        Err(err) => tracing::debug!("dial {addr} failed: {err}"),
    }
}

fn lock_pid(paths: &Paths) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&paths.pid_file)
        .with_context(|| format!("opening {}", paths.pid_file.display()))?;
    if file.try_lock_exclusive().is_err() {
        if let Some(pid) = running_pid(paths) {
            anyhow::bail!("pastebridge is already running (pid {pid})");
        }
        anyhow::bail!("pastebridge is already running");
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(file)
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
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

struct PidGuard {
    _file: std::fs::File,
    path: std::path::PathBuf,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
