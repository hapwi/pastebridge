use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use quinn::{Connection, Endpoint};
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
use tracing::warn;

use crate::config::{parse_addr, Config, Paths};
use crate::crypto::{device_id_from_cert, sas_code};
use crate::discovery::{self, TailscaleNetwork, PAIR_SERVICE};
use crate::identity::{Identity, Peer, PeerList};
use crate::protocol::{self, Msg, PROTO};
use crate::tls::{self, peer_cert};

pub async fn run(
    cfg: &Config,
    paths: &Paths,
    identity: &Identity,
    connect: Option<String>,
) -> Result<()> {
    let connecting = connect.is_some();
    let tailscale = if connecting {
        None
    } else {
        match discovery::tailscale_network() {
            Ok(network) => network,
            Err(err) => {
                warn!("Tailscale detection unavailable: {err}");
                None
            }
        }
    };
    let mdns = if connecting {
        None
    } else {
        discovery::start_mdns().ok()
    };
    let advertised = if let Some(mdns) = mdns.as_ref() {
        match discovery::advertise(
            mdns,
            PAIR_SERVICE,
            &identity.device_id,
            &identity.name,
            cfg.pair_port,
        ) {
            Ok(_) => true,
            Err(err) => {
                warn!("mDNS advertise failed: {err}");
                false
            }
        }
    } else {
        false
    };

    let client_cfg = tls::pairing_client(identity)?;
    let (endpoint, client_endpoint) = if connecting {
        let mut ep = Endpoint::client("0.0.0.0:0".parse()?)?;
        ep.set_default_client_config(client_cfg);
        (ep.clone(), ep)
    } else {
        let server = tls::pairing_server(identity)?;
        let bind = SocketAddr::from((cfg.bind_address, cfg.pair_port));
        let ep = Endpoint::server(server, bind)?;
        // Outgoing pairing uses a separate ephemeral socket. Reusing the
        // listener port as the client source breaks QUIC through Tailscale
        // and host firewalls.
        let mut client_endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        client_endpoint.set_default_client_config(client_cfg);
        (ep, client_endpoint)
    };

    println!();
    println!("  Device : {} ({})", identity.name, identity.device_id);
    if !connecting {
        println!("  Listen : {}:{}", cfg.bind_address, cfg.pair_port);
    }
    if advertised {
        println!("  LAN    : advertising via mDNS");
    }
    if let Some(network) = tailscale.as_ref() {
        println!(
            "  Tailnet: {} ({} online peer address{})",
            network.local_ip,
            network.peer_ips.len(),
            if network.peer_ips.len() == 1 {
                ""
            } else {
                "es"
            }
        );
    }
    if !connecting {
        println!();
        println!("  On the other computer, run the same command:");
        println!("      pastebridge pair");
        if tailscale.is_none() {
            println!();
            println!("  For another network, install and start Tailscale on both computers");
            println!(
                "  or use: pastebridge pair --connect <address>:{}",
                cfg.pair_port
            );
        }
        println!();
        println!("  Waiting for the other computer…  (Ctrl+C to cancel)");
        if let Some(network) = tailscale.as_ref() {
            println!("  If this sits here, on the other computer run:");
            println!(
                "      pastebridge pair --connect {}:{}",
                network.local_ip, cfg.pair_port
            );
        }
        println!();
    }

    let our_id = identity.device_id.clone();
    let (conn, as_client) = if let Some(target) = connect {
        let addr = resolve_addr(&target, cfg.pair_port)?;
        println!("  Connecting to {addr}…");
        (connect_to(&client_endpoint, addr).await?, true)
    } else {
        wait_for_peer(
            &endpoint,
            &client_endpoint,
            mdns.as_ref(),
            &our_id,
            cfg.pair_port,
            tailscale,
        )
        .await?
    };

    let peer_der = peer_cert(&conn)?;
    let our_der = identity.cert_der()?;
    let sas = sas_code(our_der.as_ref(), peer_der.as_ref());
    let peer_id = device_id_from_cert(peer_der.as_ref());

    let (mut send, mut recv) = if as_client {
        conn.open_bi().await?
    } else {
        conn.accept_bi().await?
    };
    protocol::write_msg(
        &mut send,
        &Msg::Hello {
            device_id: identity.device_id.clone(),
            name: identity.name.clone(),
            proto: PROTO,
        },
    )
    .await?;

    let hello = tokio::time::timeout(Duration::from_secs(20), protocol::read_msg(&mut recv))
        .await
        .context("timed out waiting for hello")??;
    let (peer_hello_id, peer_name) = match hello {
        Msg::Hello {
            device_id,
            name,
            proto,
        } => {
            if proto != PROTO {
                bail!("protocol mismatch (theirs {proto}, ours {PROTO})");
            }
            (device_id, name)
        }
        _ => bail!("expected hello"),
    };
    validate_peer_name(&peer_name)?;

    if peer_hello_id != peer_id {
        bail!("peer identity does not match its TLS certificate");
    }

    println!();
    println!("  Found  : {peer_name} ({peer_id})");
    println!();
    println!("  ┌─────────────────────┐");
    println!("  │  Code  {sas}  │");
    println!("  └─────────────────────┘");
    println!();
    println!("  Look at the other screen. The codes must match.");
    print!("  Pair this computer with {peer_name}? [y/N] ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let answer = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        line
    })
    .await
    .unwrap_or_default();
    let confirmed = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");

    if !confirmed {
        let _ = protocol::write_msg(&mut send, &Msg::PairAbort).await;
        bail!("pairing cancelled");
    }

    protocol::write_msg(&mut send, &Msg::PairConfirm).await?;
    println!("  Waiting for the other computer to confirm…");

    let reply = tokio::time::timeout(Duration::from_secs(90), protocol::read_msg(&mut recv))
        .await
        .context("timed out waiting for the other computer to confirm")??;

    match reply {
        Msg::PairConfirm => {}
        Msg::PairAbort => bail!("the other computer cancelled pairing"),
        other => bail!("unexpected reply: {other:?}"),
    }

    let cert_pem = der_to_pem(peer_der.as_ref())?;
    let mut peers = PeerList::load(paths)?;
    peers.upsert(Peer {
        device_id: peer_id.clone(),
        name: peer_name.clone(),
        cert_sha256: crate::crypto::cert_fingerprint(peer_der.as_ref()),
        cert_pem,
        last_addr: Some(SocketAddr::new(conn.remote_address().ip(), cfg.port).to_string()),
        paired_at: now_rfc3339(),
    });
    peers.save(paths)?;

    println!();
    println!("  Paired with {peer_name}.");
    println!("  Start the daemon on both computers:");
    println!("      pastebridge start");
    println!("  Or install so it starts when you log in:");
    println!("      pastebridge install-service");
    println!();

    let _ = mdns;
    Ok(())
}

fn should_dial_ip(local: Ipv4Addr, peer: Ipv4Addr) -> bool {
    local < peer
}

fn should_dial_device(our_id: &str, their_id: &str) -> bool {
    our_id < their_id
}

async fn wait_for_peer(
    endpoint: &Endpoint,
    client_endpoint: &Endpoint,
    mdns: Option<&mdns_sd::ServiceDaemon>,
    our_id: &str,
    pair_port: u16,
    tailscale: Option<TailscaleNetwork>,
) -> Result<(Connection, bool)> {
    let (tx, mut rx) = mpsc::channel::<(Connection, bool)>(8);
    let selected = Arc::new(AtomicBool::new(false));
    let targets: Arc<Mutex<HashSet<SocketAddr>>> = Arc::new(Mutex::new(HashSet::new()));

    let ep = endpoint.clone();
    let tx_in = tx.clone();
    let incoming_selected = selected.clone();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            match incoming.await {
                Ok(conn) => {
                    if incoming_selected
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        let _ = tx_in.send((conn, false)).await;
                        return;
                    }
                    conn.close(0u32.into(), b"another pairing connection was selected");
                }
                Err(err) => warn!("incoming pairing handshake failed: {err}"),
            }
        }
    });

    if let Some(network) = &tailscale {
        if let Ok(mut guard) = targets.lock() {
            for peer_ip in &network.peer_ips {
                if should_dial_ip(network.local_ip, *peer_ip) {
                    guard.insert(SocketAddr::from((*peer_ip, pair_port)));
                }
            }
        }
    }

    if let Some(mdns) = mdns {
        if let Ok(mut browse) = discovery::browse(mdns, PAIR_SERVICE) {
            let our_id = our_id.to_string();
            let targets = targets.clone();
            let selected = selected.clone();
            tokio::spawn(async move {
                while let Some(found) = browse.recv().await {
                    if selected.load(Ordering::Acquire) {
                        return;
                    }
                    if found.device_id == our_id {
                        continue;
                    }
                    if should_dial_device(&our_id, &found.device_id) {
                        println!(
                            "  Found {} on the LAN at {}; connecting…",
                            found.name, found.addr
                        );
                        if let Ok(mut guard) = targets.lock() {
                            guard.insert(found.addr);
                        }
                    } else {
                        println!(
                            "  Found {} on the LAN; waiting for them to connect",
                            found.name
                        );
                    }
                }
            });
        }
    }

    let elected = targets
        .lock()
        .ok()
        .is_some_and(|guard| !guard.is_empty());
    if !elected {
        let fallback_targets = targets.clone();
        let fallback_selected = selected.clone();
        let fallback_peers = tailscale.as_ref().map(|network| network.peer_ips.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            if fallback_selected.load(Ordering::Acquire) {
                return;
            }
            let Some(peer_ips) = fallback_peers else {
                return;
            };
            if peer_ips.is_empty() {
                return;
            }
            println!("  Still waiting; trying Tailscale from this side…");
            if let Ok(mut guard) = fallback_targets.lock() {
                for peer_ip in peer_ips {
                    guard.insert(SocketAddr::from((peer_ip, pair_port)));
                }
            }
        });
    }

    let dial_endpoint = client_endpoint.clone();
    let dial_tx = tx.clone();
    let dial_selected = selected.clone();
    let dial_targets = targets.clone();
    tokio::spawn(async move {
        let mut announced = HashSet::new();
        let mut reported_fail = HashSet::new();
        loop {
            if dial_selected.load(Ordering::Acquire) {
                return;
            }
            let addrs = match dial_targets.lock() {
                Ok(guard) => guard.iter().copied().collect::<Vec<_>>(),
                Err(_) => Vec::new(),
            };
            let mut attempts = tokio::task::JoinSet::new();
            for addr in addrs {
                if announced.insert(addr) {
                    println!("  Trying {addr}…");
                }
                let endpoint = dial_endpoint.clone();
                attempts.spawn(async move {
                    let result = tokio::time::timeout(
                        Duration::from_secs(6),
                        connect_to(&endpoint, addr),
                    )
                    .await;
                    (addr, result)
                });
            }
            while let Some(joined) = attempts.join_next().await {
                let Ok((addr, connected)) = joined else {
                    continue;
                };
                if dial_selected.load(Ordering::Acquire) {
                    if let Ok(Ok(conn)) = connected {
                        conn.close(0u32.into(), b"another pairing connection was selected");
                    }
                    return;
                }
                match connected {
                    Ok(Ok(conn)) => {
                        if dial_selected
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            let _ = dial_tx.send((conn, true)).await;
                            return;
                        }
                        conn.close(0u32.into(), b"another pairing connection was selected");
                        return;
                    }
                    Ok(Err(err)) => {
                        if reported_fail.insert(addr) {
                            println!("  Could not reach {addr} yet; retrying…");
                        }
                        warn!("connect to {addr} failed: {err}");
                    }
                    Err(_) => {
                        if reported_fail.insert(addr) {
                            println!("  Could not reach {addr} yet; retrying…");
                        }
                        warn!("connect to {addr} timed out");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    tokio::time::timeout(Duration::from_secs(300), rx.recv())
        .await
        .context("timed out waiting for a pairing peer")?
        .context("pairing cancelled")
}

async fn connect_to(endpoint: &Endpoint, addr: SocketAddr) -> Result<Connection> {
    endpoint
        .connect(addr, "pastebridge")?
        .await
        .with_context(|| format!("connecting to {addr}"))
}

fn resolve_addr(s: &str, default_port: u16) -> Result<SocketAddr> {
    if let Ok(addr) = parse_addr(s, default_port) {
        return Ok(addr);
    }
    let hostport = if s.contains(':') {
        s.to_string()
    } else {
        format!("{s}:{default_port}")
    };
    hostport
        .to_socket_addrs()?
        .next()
        .with_context(|| format!("could not resolve {s}"))
}

fn der_to_pem(der: &[u8]) -> Result<String> {
    let b64 = STANDARD.encode(der);
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk)?);
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    Ok(pem)
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

fn validate_peer_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        bail!("peer sent an invalid device name");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{should_dial_device, should_dial_ip};
    use std::net::Ipv4Addr;

    #[test]
    fn smaller_tailscale_ip_dials() {
        let mac = Ipv4Addr::new(100, 86, 163, 95);
        let fedora = Ipv4Addr::new(100, 118, 57, 56);
        assert!(should_dial_ip(mac, fedora));
        assert!(!should_dial_ip(fedora, mac));
    }

    #[test]
    fn smaller_device_id_dials() {
        assert!(should_dial_device("726fee597cbe41c6", "b1c67afe4a8c044f"));
        assert!(!should_dial_device("b1c67afe4a8c044f", "726fee597cbe41c6"));
    }
}
