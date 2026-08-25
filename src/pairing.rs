use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use quinn::{Connection, Endpoint};
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::{parse_addr, Config, Paths};
use crate::crypto::{device_id_from_cert, sas_code};
use crate::discovery::{self, TailscaleNetwork, PAIR_SERVICE};
use crate::identity::{Identity, Peer, PeerList};
use crate::protocol::{self, Msg, PROTO};
use crate::tls::{self, peer_cert};
use crate::ui;

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
    if let Some(mdns) = mdns.as_ref() {
        if let Err(err) = discovery::advertise(
            mdns,
            PAIR_SERVICE,
            &identity.device_id,
            &identity.name,
            cfg.pair_port,
        ) {
            warn!("mDNS advertise failed: {err}");
        }
    }

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
    println!("  {}", identity.name);

    let our_id = identity.device_id.clone();
    let looking = ui::spinner("waiting for the other computer");
    let conn = if let Some(target) = connect {
        looking.set_message(format!("connecting to {target}"));
        let addr = resolve_addr(&target, cfg.pair_port)?;
        let conn = connect_to(&client_endpoint, addr).await;
        ui::stop(looking);
        conn?
    } else {
        let conn = wait_for_peer(
            &endpoint,
            &client_endpoint,
            mdns.as_ref(),
            &our_id,
            cfg.pair_port,
            tailscale,
            &looking,
        )
        .await;
        ui::stop(looking);
        conn?
    };

    let peer_der = peer_cert(&conn)?;
    let our_der = identity.cert_der()?;
    let sas = sas_code(our_der.as_ref(), peer_der.as_ref());
    let peer_id = device_id_from_cert(peer_der.as_ref());

    let (mut send, mut recv) = if we_open_pairing_stream(&our_id, &peer_id) {
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
    println!("  {peer_name}");
    println!();
    println!("     {sas}");
    println!();
    print!("  codes match? [y/N] ");
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
    let waiting = ui::spinner(format!("waiting for {peer_name}"));
    let reply = tokio::time::timeout(Duration::from_secs(90), protocol::read_msg(&mut recv)).await;
    ui::stop(waiting);
    let reply = reply.context("timed out waiting for the other computer to confirm")??;

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

    println!("  paired with {peer_name}");
    if crate::daemon::running_pid(paths).is_none() {
        println!("  start syncing with  pastebridge start");
    }
    println!();

    let _ = mdns;
    Ok(())
}

fn we_open_pairing_stream(our_id: &str, peer_id: &str) -> bool {
    our_id < peer_id
}

fn preferred_pairing_path(our_id: &str, peer_id: &str, we_initiated: bool) -> bool {
    we_initiated == (our_id < peer_id)
}

async fn offer_connection(
    conn: Connection,
    we_initiated: bool,
    our_id: &str,
    tx: mpsc::Sender<Connection>,
    selected: Arc<AtomicBool>,
) {
    let peer_id = match peer_cert(&conn) {
        Ok(der) => device_id_from_cert(der.as_ref()),
        Err(err) => {
            warn!("pairing connection had no certificate: {err}");
            conn.close(0u32.into(), b"missing certificate");
            return;
        }
    };
    if peer_id == our_id {
        conn.close(0u32.into(), b"connected to self");
        return;
    }
    if !preferred_pairing_path(our_id, &peer_id, we_initiated) {
        tokio::time::sleep(Duration::from_millis(2500)).await;
        if selected.load(Ordering::Acquire) {
            conn.close(0u32.into(), b"another pairing connection was selected");
            return;
        }
    }
    if selected
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let _ = tx.send(conn).await;
    } else {
        conn.close(0u32.into(), b"another pairing connection was selected");
    }
}

async fn wait_for_peer(
    endpoint: &Endpoint,
    client_endpoint: &Endpoint,
    mdns: Option<&mdns_sd::ServiceDaemon>,
    our_id: &str,
    pair_port: u16,
    tailscale: Option<TailscaleNetwork>,
    looking: &indicatif::ProgressBar,
) -> Result<Connection> {
    let looking = looking.clone();
    let (tx, mut rx) = mpsc::channel::<Connection>(8);
    let selected = Arc::new(AtomicBool::new(false));
    let targets: Arc<Mutex<HashMap<SocketAddr, String>>> = Arc::new(Mutex::new(HashMap::new()));

    if let Some(network) = &tailscale {
        if let Ok(mut guard) = targets.lock() {
            for peer in network.pairable_peers() {
                guard
                    .entry(SocketAddr::from((peer.ip, pair_port)))
                    .or_insert_with(|| peer.name.clone());
            }
        }
    }

    let ep = endpoint.clone();
    let tx_in = tx.clone();
    let incoming_selected = selected.clone();
    let incoming_id = our_id.to_string();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            if incoming_selected.load(Ordering::Acquire) {
                return;
            }
            match incoming.await {
                Ok(conn) => {
                    let tx_in = tx_in.clone();
                    let incoming_selected = incoming_selected.clone();
                    let incoming_id = incoming_id.clone();
                    tokio::spawn(async move {
                        offer_connection(conn, false, &incoming_id, tx_in, incoming_selected).await;
                    });
                }
                Err(err) => warn!("incoming pairing handshake failed: {err}"),
            }
        }
    });

    if let Some(mdns) = mdns {
        if let Ok(mut browse) = discovery::browse(mdns, PAIR_SERVICE) {
            let our_id = our_id.to_string();
            let targets = targets.clone();
            let selected = selected.clone();
            let looking = looking.clone();
            tokio::spawn(async move {
                while let Some(found) = browse.recv().await {
                    if selected.load(Ordering::Acquire) {
                        return;
                    }
                    if found.device_id == our_id {
                        continue;
                    }
                    looking.set_message(format!("found {} on the LAN", found.name));
                    if let Ok(mut guard) = targets.lock() {
                        guard
                            .entry(found.addr)
                            .or_insert_with(|| found.name.clone());
                    }
                }
            });
        }
    }

    if tailscale.is_some() {
        let refresh_targets = targets.clone();
        let refresh_selected = selected.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                if refresh_selected.load(Ordering::Acquire) {
                    return;
                }
                let network = tokio::task::spawn_blocking(discovery::tailscale_network).await;
                let Ok(Ok(Some(network))) = network else {
                    continue;
                };
                if let Ok(mut guard) = refresh_targets.lock() {
                    for peer in network.pairable_peers() {
                        guard
                            .entry(SocketAddr::from((peer.ip, pair_port)))
                            .or_insert_with(|| peer.name.clone());
                    }
                }
            }
        });
    }

    let dial_endpoint = client_endpoint.clone();
    let dial_tx = tx.clone();
    let dial_selected = selected.clone();
    let dial_targets = targets;
    let dial_id = our_id.to_string();
    let dial_looking = looking;
    tokio::spawn(async move {
        let mut announced = HashMap::new();
        let mut warmed = HashMap::new();
        loop {
            if dial_selected.load(Ordering::Acquire) {
                return;
            }
            let addrs = match dial_targets.lock() {
                Ok(guard) => guard
                    .iter()
                    .map(|(addr, name)| (*addr, name.clone()))
                    .collect::<Vec<_>>(),
                Err(_) => Vec::new(),
            };
            let mut attempts = tokio::task::JoinSet::new();
            for (addr, name) in addrs {
                if announced.insert(addr, name.clone()).is_none() {
                    dial_looking.set_message(format!("trying {name}"));
                    if let std::net::IpAddr::V4(ip) = addr.ip() {
                        if warmed.insert(ip, ()).is_none() {
                            tokio::task::spawn_blocking(move || discovery::tailscale_ping(ip));
                        }
                    }
                }
                let endpoint = dial_endpoint.clone();
                attempts.spawn(async move {
                    let result =
                        tokio::time::timeout(Duration::from_secs(8), connect_to(&endpoint, addr))
                            .await;
                    (addr, name, result)
                });
            }
            while let Some(joined) = attempts.join_next().await {
                let Ok((addr, name, connected)) = joined else {
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
                        let tx = dial_tx.clone();
                        let selected = dial_selected.clone();
                        let our_id = dial_id.clone();
                        tokio::spawn(async move {
                            offer_connection(conn, true, &our_id, tx, selected).await;
                        });
                    }
                    Ok(Err(err)) => debug!("connect to {name} ({addr}) failed: {err}"),
                    Err(_) => debug!("connect to {name} ({addr}) timed out"),
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
    use super::{preferred_pairing_path, we_open_pairing_stream};

    #[test]
    fn smaller_device_id_opens_the_pairing_stream() {
        assert!(we_open_pairing_stream(
            "726fee597cbe41c6",
            "b1c67afe4a8c044f"
        ));
        assert!(!we_open_pairing_stream(
            "b1c67afe4a8c044f",
            "726fee597cbe41c6"
        ));
    }

    #[test]
    fn smaller_device_id_is_the_preferred_initiator() {
        let mac = "726fee597cbe41c6";
        let fedora = "b1c67afe4a8c044f";
        assert!(preferred_pairing_path(mac, fedora, true));
        assert!(preferred_pairing_path(fedora, mac, false));
        assert!(!preferred_pairing_path(mac, fedora, false));
        assert!(!preferred_pairing_path(fedora, mac, true));
    }
}
