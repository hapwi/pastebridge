use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use tokio::sync::{broadcast, Mutex, Semaphore};
use tracing::{debug, info, warn};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::clipboard::Clipboard;
use crate::config::{parse_addr, Config, Paths};
use crate::crypto::device_id_from_cert;
use crate::discovery::{self, SERVICE};
use crate::identity::{Identity, PeerList};
use crate::sync::{self, IncomingClipboard, OutgoingClip, SessionOptions};
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
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    pub device_id: String,
    pub name: String,
    pub connected: bool,
    pub last_addr: Option<String>,
}

#[derive(Clone)]
struct Session {
    connection: quinn::Connection,
    ready: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SessionConnector {
    endpoint: quinn::Endpoint,
    identity: Identity,
    out_tx: broadcast::Sender<OutgoingClip>,
    incoming: IncomingClipboard,
    connected: Arc<Mutex<HashMap<String, Session>>>,
    options: SessionOptions,
    last_err: Arc<Mutex<Option<String>>>,
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
    let incoming = IncomingClipboard::default();
    let connected: Arc<Mutex<HashMap<String, Session>>> = Arc::new(Mutex::new(HashMap::new()));
    let last_sent = Arc::new(Mutex::new(None::<String>));
    let last_recv = Arc::new(Mutex::new(None::<String>));
    let last_err = Arc::new(Mutex::new(None::<String>));
    let clipboard_tick = Arc::new(Mutex::new(None::<Instant>));
    let peers = Arc::new(Mutex::new(peers));

    let connector = SessionConnector {
        endpoint: client_endpoint.clone(),
        identity: identity.clone(),
        out_tx: out_tx.clone(),
        incoming: incoming.clone(),
        connected: connected.clone(),
        options: SessionOptions {
            max_payload_bytes: cfg.max_payload_bytes,
            sync_images: cfg.sync_images,
        },
        last_err: last_err.clone(),
    };
    {
        let endpoint = endpoint.clone();
        let connector = connector.clone();
        let pins = pins.clone();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let connector = connector.clone();
                let pins = pins.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(conn) => {
                            let Ok(cert) = peer_cert(&conn) else { return };
                            if !pins.contains(cert.as_ref()) {
                                conn.close(0u32.into(), b"unpinned certificate");
                                return;
                            }
                            let peer_id = device_id_from_cert(cert.as_ref());
                            if connector.identity.device_id < peer_id {
                                conn.close(0u32.into(), b"peer uses the client connection");
                                return;
                            }
                            start_session(&connector, conn, false, peer_id).await;
                        }
                        Err(err) => debug!("incoming handshake failed: {err}"),
                    }
                });
            }
        });
    }

    // mDNS browse → connect to known peers with higher device id
    if let Some(mdns) = mdns.as_ref() {
        if let Ok(mut browse) = discovery::browse(mdns, SERVICE) {
            let our_id = identity.device_id.clone();
            let connector = connector.clone();
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
        let connector = connector.clone();
        let peers = peers.clone();
        let our_id = identity.device_id.clone();
        let static_peers = cfg.static_peers.clone();
        let port = cfg.port;
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let snapshot = peers.lock().await.peers.clone();
                let mut targets = HashMap::new();
                for peer in snapshot {
                    if peer.device_id <= our_id
                        || connector
                            .connected
                            .lock()
                            .await
                            .contains_key(&peer.device_id)
                    {
                        continue;
                    }
                    if let Some(addr) = peer
                        .last_addr
                        .as_deref()
                        .and_then(|addr| parse_addr(addr, port).ok())
                    {
                        targets.insert(addr, peer.device_id);
                    }
                }
                for spec in &static_peers {
                    if let Ok(addr) = parse_addr(spec, port) {
                        targets.entry(addr).or_insert_with(|| spec.clone());
                    }
                }
                if connector.connected.lock().await.len() < peers.lock().await.peers.len() {
                    match tokio::task::spawn_blocking(discovery::tailscale_network).await {
                        Ok(Ok(Some(network))) => {
                            for peer in network.pairable_peers() {
                                let addr = SocketAddr::from((peer.ip, port));
                                targets.entry(addr).or_insert_with(|| addr.to_string());
                            }
                        }
                        Ok(Err(err)) => debug!("Tailscale discovery unavailable: {err:#}"),
                        _ => {}
                    }
                }
                let limit = Arc::new(Semaphore::new(16));
                let mut attempts = tokio::task::JoinSet::new();
                for (addr, peer_key) in targets {
                    let connector = connector.clone();
                    let limit = limit.clone();
                    attempts.spawn(async move {
                        let Ok(_permit) = limit.acquire_owned().await else {
                            return;
                        };
                        try_connect(&connector, addr, &peer_key).await;
                    });
                }
                while let Some(result) = attempts.join_next().await {
                    if let Err(err) = result {
                        warn!("reconnection task failed: {err}");
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
                        connected.lock().await.retain(|peer_id, session| {
                            let keep = list.get(peer_id).is_some();
                            if !keep {
                                session.connection.close(0u32.into(), b"peer unpaired");
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

    // Poll, apply, and expire on one real thread: NSPasteboard / Wayland types
    // are often !Send, and one owner keeps remote echo suppression effective.
    let clipboard_thread = {
        let out_tx = out_tx.clone();
        let identity = identity.clone();
        let last_sent = last_sent.clone();
        let last_recv = last_recv.clone();
        let last_err = last_err.clone();
        let clipboard_tick = clipboard_tick.clone();
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
                    *clipboard_tick.blocking_lock() = Some(Instant::now());
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
                    let Some(incoming) = incoming.recv_timeout(timeout) else {
                        continue;
                    };
                    if incoming.mime == "image/png" && !sync_images {
                        continue;
                    }
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
            .context("starting clipboard thread")?
    };

    // Status file
    {
        let paths = paths.clone();
        let identity = identity.clone();
        let peers = peers.clone();
        let connected = connected.clone();
        let last_sent = last_sent.clone();
        let last_recv = last_recv.clone();
        let last_err = last_err.clone();
        let clipboard_tick = clipboard_tick.clone();
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
                            connected: connected_set
                                .get(&p.device_id)
                                .is_some_and(|session| session.ready.load(Ordering::Acquire)),
                            device_id: p.device_id.clone(),
                            name: p.name.clone(),
                            last_addr: p.last_addr.clone(),
                        })
                        .collect(),
                    last_sent: last_sent.lock().await.clone(),
                    last_received: last_recv.lock().await.clone(),
                    last_error: last_err.lock().await.clone(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    updated_at: now(),
                    ready: clipboard_tick
                        .lock()
                        .await
                        .is_some_and(|tick| tick.elapsed() < Duration::from_secs(60)),
                };
                if let Ok(json) = serde_json::to_string_pretty(&status) {
                    let _ = crate::config::write_secret_file(&paths.status_file, &json);
                }
            }
        });
    }

    info!("pastebridge is running — copy on one computer, paste on the other");
    let worker_health = async {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let stalled = clipboard_tick
                .lock()
                .await
                .is_some_and(|tick| tick.elapsed() >= Duration::from_secs(60));
            if clipboard_thread.is_finished() || stalled {
                return Err(anyhow::anyhow!(
                    "clipboard worker stopped or became unresponsive"
                ));
            }
        }
    };
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
            failure = worker_health => return failure,
        }
    }
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
                start_session(connector, conn, true, peer_id).await;
            }
            Ok(Err(err)) => tracing::debug!("connect {addr} failed: {err}"),
            Err(_) => tracing::debug!("connect {addr} timed out"),
        },
        Err(err) => tracing::debug!("dial {addr} failed: {err}"),
    }
}

async fn start_session(
    connector: &SessionConnector,
    conn: quinn::Connection,
    as_client: bool,
    peer_id: String,
) {
    let ready = Arc::new(AtomicBool::new(false));
    {
        let mut sessions = connector.connected.lock().await;
        if sessions.contains_key(&peer_id) {
            conn.close(0u32.into(), b"duplicate connection");
            return;
        }
        sessions.insert(
            peer_id.clone(),
            Session {
                connection: conn.clone(),
                ready: ready.clone(),
            },
        );
    }
    let connector = connector.clone();
    tokio::spawn(async move {
        let result = sync::run_session(
            conn.clone(),
            as_client,
            connector.identity,
            connector.out_tx.subscribe(),
            connector.incoming,
            connector.options,
            ready,
        )
        .await;
        if let Err(err) = result {
            let detail = format!("session with {peer_id}: {err}");
            if conn.close_reason().is_some() {
                info!("peer {peer_id} disconnected; will retry");
            } else {
                warn!("{detail}");
                *connector.last_err.lock().await = Some(detail);
            }
        }
        conn.close(0u32.into(), b"session ended");
        let mut sessions = connector.connected.lock().await;
        if sessions
            .get(&peer_id)
            .is_some_and(|session| session.connection.stable_id() == conn.stable_id())
        {
            sessions.remove(&peer_id);
        }
    });
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
