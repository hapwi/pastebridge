use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub const SERVICE: &str = "_pastebridge._udp.local.";
pub const PAIR_SERVICE: &str = "_pastebridge-pair._udp.local.";
const MAX_TAILSCALE_STATUS_BYTES: usize = 2 * 1024 * 1024;
const MAX_TAILSCALE_PEERS: usize = 64;
const MAX_TAILSCALE_IPS_PER_PEER: usize = 8;

#[derive(Debug, Clone)]
pub struct FoundPeer {
    pub device_id: String,
    pub name: String,
    pub addr: SocketAddr,
}

#[derive(Debug, Clone)]
pub struct TailscalePeer {
    pub name: String,
    pub os: String,
    pub ip: Ipv4Addr,
}

impl TailscalePeer {
    pub fn is_pairable(&self) -> bool {
        is_pairable_os(&self.os)
    }
}

#[derive(Debug, Clone)]
pub struct TailscaleNetwork {
    pub local_ip: Ipv4Addr,
    pub peers: Vec<TailscalePeer>,
}

impl TailscaleNetwork {
    pub fn peer_ips(&self) -> Vec<Ipv4Addr> {
        let mut ips: Vec<_> = self.peers.iter().map(|peer| peer.ip).collect();
        ips.sort_unstable();
        ips.dedup();
        ips
    }

    pub fn pairable_peers(&self) -> impl Iterator<Item = &TailscalePeer> {
        self.peers.iter().filter(|peer| peer.is_pairable())
    }
}

#[derive(Debug, Deserialize)]
struct RawTailscaleStatus {
    #[serde(rename = "BackendState", default)]
    backend_state: String,
    #[serde(rename = "Peer", default)]
    peers: HashMap<String, RawTailscalePeer>,
}

#[derive(Debug, Deserialize)]
struct RawTailscalePeer {
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "OS", default)]
    os: String,
    #[serde(rename = "Online", default)]
    online: bool,
    #[serde(rename = "TailscaleIPs", default)]
    tailscale_ips: Vec<String>,
}

pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    if let Ok(ifas) = local_ip_address::list_afinet_netifas() {
        for (name, ip) in ifas {
            let lname = name.to_ascii_lowercase();
            if lname.starts_with("lo") || lname.contains("docker") || lname.starts_with("veth") {
                continue;
            }
            if let IpAddr::V4(v4) = ip {
                if !v4.is_loopback() && !v4.is_link_local() && !v4.is_multicast() {
                    out.push(v4);
                }
            }
        }
    }
    if out.is_empty() {
        if let Ok(IpAddr::V4(v4)) = local_ip_address::local_ip() {
            out.push(v4);
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn start_mdns() -> Result<ServiceDaemon> {
    ServiceDaemon::new().context("starting mDNS (is UDP 5353 allowed?)")
}

pub fn tailscale_network() -> Result<Option<TailscaleNetwork>> {
    let Some(binary) = tailscale_binary() else {
        return Ok(None);
    };
    let local_output = command_output_limited(&binary, &["ip", "-4"], 1024)
        .context("running `tailscale ip -4`")?;
    let local_text = std::str::from_utf8(&local_output).context("invalid tailscale IP output")?;
    let local_ip = local_text
        .lines()
        .take(4)
        .find_map(|line| line.trim().parse::<Ipv4Addr>().ok())
        .filter(is_usable_ipv4)
        .context("Tailscale did not report a usable IPv4 address")?;

    let status_output =
        command_output_limited(&binary, &["status", "--json"], MAX_TAILSCALE_STATUS_BYTES)
            .context("running `tailscale status --json`")?;
    parse_tailscale_status(&status_output, local_ip).map(Some)
}

pub fn tailscale_ping(ip: Ipv4Addr) {
    let Some(binary) = tailscale_binary() else {
        return;
    };
    let _ = command_output_limited(&binary, &["ping", "-c", "1", &ip.to_string()], 32 * 1024);
}

pub fn advertise(
    mdns: &ServiceDaemon,
    service: &str,
    device_id: &str,
    name: &str,
    port: u16,
) -> Result<ServiceInfo> {
    let host = format!(
        "{}.local.",
        hostname::get()
            .map(|h| sanitize_label(&h.to_string_lossy()))
            .unwrap_or_else(|_| "pastebridge".into())
    );
    let ips = local_ipv4s();
    if ips.is_empty() {
        anyhow::bail!("no local IPv4 address found");
    }
    let mut props = HashMap::new();
    props.insert("id".to_string(), device_id.to_string());
    props.insert("name".to_string(), name.to_string());
    props.insert("ver".to_string(), "1".to_string());

    let info = ServiceInfo::new(
        service,
        &sanitize_label(device_id),
        &host,
        "",
        port,
        Some(props),
    )?
    .enable_addr_auto();
    mdns.register(info.clone())?;
    Ok(info)
}

pub fn browse(mdns: &ServiceDaemon, service: &str) -> Result<mpsc::Receiver<FoundPeer>> {
    let receiver = mdns.browse(service)?;
    let (tx, rx) = mpsc::channel(32);
    std::thread::spawn(move || {
        while let Ok(event) = receiver.recv_timeout(Duration::from_secs(3600)) {
            if let ServiceEvent::ServiceResolved(info) = event {
                let device_id = info.get_property_val_str("id").unwrap_or("").to_string();
                let name = info
                    .get_property_val_str("name")
                    .unwrap_or(info.get_fullname())
                    .to_string();
                let port = info.get_port();
                if !valid_device_id(&device_id) || !valid_device_name(&name) || port == 0 {
                    continue;
                }
                for addr in info.get_addresses() {
                    if let IpAddr::V4(v4) = *addr {
                        if v4.is_loopback() {
                            continue;
                        }
                        let found = FoundPeer {
                            device_id: device_id.clone(),
                            name: name.clone(),
                            addr: SocketAddr::from((v4, port)),
                        };
                        if tx.blocking_send(found).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
    Ok(rx)
}

fn sanitize_label(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.is_empty() {
        out = "pastebridge".into();
    }
    out.truncate(63);
    out
}

fn parse_tailscale_status(bytes: &[u8], local_ip: Ipv4Addr) -> Result<TailscaleNetwork> {
    let status: RawTailscaleStatus =
        serde_json::from_slice(bytes).context("parsing Tailscale status")?;
    if status.backend_state != "Running" {
        anyhow::bail!("Tailscale backend is not running");
    }
    if status.peers.len() > MAX_TAILSCALE_PEERS {
        anyhow::bail!("Tailscale status contains too many peers");
    }

    let mut peers = Vec::new();
    for peer in status.peers.into_values() {
        if !peer.online || peer.tailscale_ips.len() > MAX_TAILSCALE_IPS_PER_PEER {
            continue;
        }
        let name = if peer.host_name.trim().is_empty() {
            "tailscale-peer".to_string()
        } else {
            peer.host_name
        };
        for raw_ip in peer.tailscale_ips {
            if raw_ip.len() > 45 {
                continue;
            }
            let Ok(ip) = raw_ip.parse::<Ipv4Addr>() else {
                continue;
            };
            if ip != local_ip && is_usable_ipv4(&ip) {
                peers.push(TailscalePeer {
                    name: name.clone(),
                    os: peer.os.clone(),
                    ip,
                });
            }
        }
    }
    peers.sort_by_key(|peer| peer.ip);
    peers.dedup_by_key(|peer| peer.ip);
    Ok(TailscaleNetwork { local_ip, peers })
}

pub fn is_pairable_os(os: &str) -> bool {
    let os = os.to_ascii_lowercase();
    !(os.contains("ios") || os.contains("android") || os.contains("tvos") || os.contains("watchos"))
}

fn command_output_limited(binary: &Path, args: &[&str], max_bytes: usize) -> Result<Vec<u8>> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().context("capturing tailscale output")?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let result = stdout
            .by_ref()
            .take(max_bytes.saturating_add(1) as u64)
            .read_to_end(&mut output);
        (result, output)
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            anyhow::bail!("tailscale command timed out");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let (read_result, output) = reader
        .join()
        .map_err(|_| anyhow::anyhow!("tailscale output reader panicked"))?;
    read_result?;
    if output.len() > max_bytes {
        anyhow::bail!("tailscale output exceeded the safety limit");
    }
    if !status.success() {
        anyhow::bail!("tailscale command failed");
    }
    Ok(output)
}

fn tailscale_binary() -> Option<PathBuf> {
    if let Some(binary) = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join("tailscale"))
            .find(|path| path.is_file())
    }) {
        return Some(binary);
    }
    [
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
        "/usr/bin/tailscale",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

fn is_usable_ipv4(ip: &Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_broadcast()
}

fn valid_device_id(device_id: &str) -> bool {
    device_id.len() == 16 && device_id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_device_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_online_tailscale_ipv4_peers() {
        let status = br#"{
            "BackendState": "Running",
            "Peer": {
                "node-a": {
                    "Online": true,
                    "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"]
                },
                "node-b": {
                    "Online": false,
                    "TailscaleIPs": ["100.64.0.3"]
                }
            }
        }"#;
        let network = parse_tailscale_status(status, Ipv4Addr::new(100, 64, 0, 1)).unwrap();
        assert_eq!(network.peer_ips(), vec![Ipv4Addr::new(100, 64, 0, 2)]);
    }

    #[test]
    fn skips_phones_when_listing_pairable_peers() {
        let status = br#"{
            "BackendState": "Running",
            "Peer": {
                "phone": {
                    "HostName": "localhost",
                    "OS": "iOS",
                    "Online": true,
                    "TailscaleIPs": ["100.81.134.90"]
                },
                "mac": {
                    "HostName": "Petes-MacBook-Air",
                    "OS": "macOS",
                    "Online": true,
                    "TailscaleIPs": ["100.86.163.95"]
                }
            }
        }"#;
        let network = parse_tailscale_status(status, Ipv4Addr::new(100, 118, 57, 56)).unwrap();
        let pairable: Vec<_> = network.pairable_peers().map(|peer| peer.ip).collect();
        assert_eq!(pairable, vec![Ipv4Addr::new(100, 86, 163, 95)]);
    }

    #[test]
    fn rejects_non_running_tailscale_status() {
        let status = br#"{"BackendState":"Stopped","Peer":{}}"#;
        assert!(parse_tailscale_status(status, Ipv4Addr::new(100, 64, 0, 1)).is_err());
    }
}
