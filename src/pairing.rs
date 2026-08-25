use anyhow::{bail, Context, Result};
use base64::Engine;
use quinn::{Connection, Endpoint};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::{parse_addr, Config, Paths};
use crate::crypto::{device_id_from_cert, sas_code};
use crate::discovery::{self, PAIR_SERVICE};
use crate::identity::{Identity, Peer, PeerList};
use crate::protocol::{self, Msg, PROTO};
use crate::tls::{self, peer_cert};

pub async fn run(
    cfg: &Config,
    paths: &Paths,
    identity: &Identity,
    connect: Option<String>,
    yes: bool,
) -> Result<()> {
    let connecting = connect.is_some();
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
        let bind: SocketAddr = format!("0.0.0.0:{}", cfg.pair_port).parse()?;
        let ep = Endpoint::server(server, bind)?;
        let mut client_endpoint = ep.clone();
        client_endpoint.set_default_client_config(client_cfg);
        (ep, client_endpoint)
    };

    println!();
    println!("  Device : {} ({})", identity.name, identity.device_id);
    if !connecting {
        println!("  Listen : 0.0.0.0:{}", cfg.pair_port);
    }
    if advertised {
        println!("  LAN    : advertising via mDNS");
    }
    if !connecting {
        println!();
        println!("  On the other computer, run the same command:");
        println!("      pastebridge pair");
        if let Some(ip) = discovery::local_ipv4s().first() {
            println!();
            println!("  If the computers are not on the same LAN (Tailscale, VPN):");
            println!("      pastebridge pair --connect {ip}:{}", cfg.pair_port);
        }
        println!();
        println!("  Waiting for the other computer…  (Ctrl+C to cancel)");
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

    if peer_hello_id != peer_id && !peer_hello_id.is_empty() {
        // Prefer the id derived from the cert; hello is informational.
        tracing::debug!("hello id {peer_hello_id} vs cert id {peer_id}");
    }

    println!();
    println!("  Found  : {peer_name} ({peer_id})");
    println!();
    println!("  ┌─────────────────────┐");
    println!("  │  Code  {sas}  │");
    println!("  └─────────────────────┘");
    println!();
    println!("  Look at the other screen. The codes must match.");
    let confirmed = if yes {
        println!("  Auto-confirming (--yes).");
        true
    } else {
        print!("  Pair this computer with {peer_name}? [y/N] ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let answer = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok();
            line
        })
        .await
        .unwrap_or_default();
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };

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
        last_addr: conn.remote_address().to_string().into(),
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

async fn wait_for_peer(
    endpoint: &Endpoint,
    client_endpoint: &Endpoint,
    mdns: Option<&mdns_sd::ServiceDaemon>,
    our_id: &str,
    pair_port: u16,
) -> Result<(Connection, bool)> {
    let (tx, mut rx) = mpsc::channel::<(Connection, bool)>(4);

    let ep = endpoint.clone();
    let tx_in = tx.clone();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            match incoming.await {
                Ok(conn) => {
                    let _ = tx_in.send((conn, false)).await;
                    return;
                }
                Err(err) => warn!("incoming pairing handshake failed: {err}"),
            }
        }
    });

    if let Some(mdns) = mdns {
        if let Ok(mut browse) = discovery::browse(mdns, PAIR_SERVICE) {
            let our_id = our_id.to_string();
            let client_endpoint = client_endpoint.clone();
            tokio::spawn(async move {
                while let Some(found) = browse.recv().await {
                    if found.device_id == our_id {
                        continue;
                    }
                    if found.device_id < our_id {
                        info!(
                            "discovered {} ({}); waiting for them to connect",
                            found.name, found.device_id
                        );
                        continue;
                    }
                    info!(
                        "discovered {} ({}) at {}; connecting",
                        found.name, found.device_id, found.addr
                    );
                    match connect_to(&client_endpoint, found.addr).await {
                        Ok(conn) => {
                            let _ = tx.send((conn, true)).await;
                            return;
                        }
                        Err(err) => warn!("connect to {} failed: {err}", found.addr),
                    }
                }
            });
        }
    } else {
        let _ = pair_port;
    }

    tokio::time::timeout(Duration::from_secs(300), rx.recv())
        .await
        .context("timed out waiting for a pairing peer")?
        .context("pairing cancelled")
}

async fn connect_to(endpoint: &Endpoint, addr: SocketAddr) -> Result<Connection> {
    Ok(endpoint
        .connect(addr, "pastebridge")?
        .await
        .with_context(|| format!("connecting to {addr}"))?)
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
    use base64::engine::general_purpose::STANDARD;
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
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}
