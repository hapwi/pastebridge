use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::sync::mpsc;

pub const SERVICE: &str = "_pastebridge._udp.local.";
pub const PAIR_SERVICE: &str = "_pastebridge-pair._udp.local.";

#[derive(Debug, Clone)]
pub struct FoundPeer {
    pub device_id: String,
    pub name: String,
    pub addr: SocketAddr,
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
