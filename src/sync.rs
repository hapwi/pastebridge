use anyhow::{bail, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use quinn::{Connection, RecvStream, SendStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tracing::{debug, info};

use crate::crypto::{clip_hash, device_id_from_cert};
use crate::identity::Identity;
use crate::protocol::{self, Msg, PROTO};
use crate::tls::peer_cert;

#[derive(Debug, Clone)]
pub struct OutgoingClip {
    pub origin: String,
    pub mime: String,
    pub hash: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct IncomingClip {
    pub from: String,
    pub origin: String,
    pub mime: String,
    pub hash: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct IncomingClipboard {
    pending: Arc<(Mutex<Option<IncomingClip>>, Condvar)>,
}

impl IncomingClipboard {
    pub fn publish(&self, clip: IncomingClip) {
        let (slot, changed) = &*self.pending;
        *slot.lock().unwrap_or_else(|err| err.into_inner()) = Some(clip);
        changed.notify_one();
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<IncomingClip> {
        let (slot, changed) = &*self.pending;
        let pending = slot.lock().unwrap_or_else(|err| err.into_inner());
        let (mut pending, _) = changed
            .wait_timeout_while(pending, timeout, |pending| pending.is_none())
            .unwrap_or_else(|err| err.into_inner());
        pending.take()
    }
}

#[derive(Clone, Copy)]
pub struct SessionOptions {
    pub max_payload_bytes: usize,
    pub sync_images: bool,
}

pub async fn run_session(
    conn: Connection,
    as_client: bool,
    identity: Identity,
    mut outgoing: broadcast::Receiver<OutgoingClip>,
    incoming: IncomingClipboard,
    options: SessionOptions,
    ready: Arc<AtomicBool>,
) -> Result<()> {
    let peer_addr = conn.remote_address();
    let (mut send, mut recv, peer_id, peer_name) = tokio::time::timeout(
        Duration::from_secs(10),
        exchange_hello(&conn, as_client, &identity),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out waiting for peer hello"))??;
    ready.store(true, Ordering::Release);
    info!("connected to {peer_name} ({peer_id}) at {peer_addr}");

    let mut reader = protocol::MessageReader::default();
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ping.tick() => {
                write_session_msg(&mut send, &Msg::Ping).await?;
            }
            clip = outgoing.recv() => {
                match clip {
                    Ok(clip) => {
                        if clip.origin == peer_id {
                            continue;
                        }
                        let msg = Msg::Clip {
                            origin: clip.origin,
                            mime: clip.mime,
                            hash: clip.hash,
                            data_b64: STANDARD.encode(&clip.bytes),
                        };
                        write_session_msg(&mut send, &msg).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("session to {peer_name} lagged {n} clips");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = reader.read(&mut recv) => {
                match msg? {
                    Msg::Ping => write_session_msg(&mut send, &Msg::Pong).await?,
                    Msg::Pong => {},
                    Msg::Clip { origin, mime, hash, data_b64 } => {
                        let bytes = decode_incoming_clip(
                            &peer_id, &origin, &mime, &hash, &data_b64,
                            options.max_payload_bytes,
                        )
                        .map_err(|err| anyhow::anyhow!("bad clip from {peer_name}: {err}"))?;
                        if mime == "image/png" && !options.sync_images {
                            continue;
                        }
                        incoming.publish(IncomingClip {
                            from: peer_id.clone(), origin, mime, hash, bytes,
                        });
                    }
                    Msg::Hello { .. } => {},
                    other => debug!("ignored {} from {peer_name}", message_kind(&other)),
                }
            }
        }
    }
    Ok(())
}

async fn write_session_msg(send: &mut SendStream, msg: &Msg) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), protocol::write_msg(send, msg))
        .await
        .map_err(|_| anyhow::anyhow!("peer stopped accepting messages"))?
}

async fn exchange_hello(
    conn: &Connection,
    as_client: bool,
    identity: &Identity,
) -> Result<(SendStream, RecvStream, String, String)> {
    let peer_addr = conn.remote_address();
    let authenticated_peer_id = device_id_from_cert(peer_cert(&conn)?.as_ref());
    let (mut send, mut recv): (SendStream, RecvStream) = if as_client {
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

    let hello = protocol::read_msg(&mut recv).await?;
    let (hello_peer_id, peer_name) = match hello {
        Msg::Hello {
            device_id,
            name,
            proto,
        } => {
            if proto != PROTO {
                anyhow::bail!("protocol mismatch with {name}");
            }
            (device_id, name)
        }
        _ => anyhow::bail!("expected hello from {peer_addr}"),
    };
    if hello_peer_id != authenticated_peer_id {
        bail!("peer identity does not match its TLS certificate");
    }
    if peer_name.is_empty() || peer_name.len() > 128 || peer_name.chars().any(char::is_control) {
        bail!("peer sent an invalid device name");
    }
    let peer_id = authenticated_peer_id;

    Ok((send, recv, peer_id, peer_name))
}

fn message_kind(message: &Msg) -> &'static str {
    match message {
        Msg::Hello { .. } => "hello",
        Msg::PairConfirm => "pair_confirm",
        Msg::PairAbort => "pair_abort",
        Msg::Clip { .. } => "clip",
        Msg::Ping => "ping",
        Msg::Pong => "pong",
    }
}

fn decode_incoming_clip(
    peer_id: &str,
    origin: &str,
    mime: &str,
    hash: &str,
    data_b64: &str,
    max_payload_bytes: usize,
) -> Result<Vec<u8>> {
    if origin != peer_id {
        bail!("spoofed origin");
    }
    if !matches!(mime, "text/plain" | "image/png") {
        bail!("unsupported clipboard type");
    }
    let max_encoded = max_payload_bytes.div_ceil(3).saturating_mul(4);
    if data_b64.len() > max_encoded {
        bail!("oversized clipboard item");
    }
    let bytes = STANDARD.decode(data_b64)?;
    if bytes.len() > max_payload_bytes {
        bail!("oversized clipboard item");
    }
    if hash != clip_hash(mime, &bytes) {
        bail!("invalid clipboard hash");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_clipboard_keeps_only_the_latest_copy() {
        let pending = IncomingClipboard::default();
        for text in ["old", "latest"] {
            pending.publish(IncomingClip {
                from: "peer".into(),
                origin: "peer".into(),
                mime: "text/plain".into(),
                hash: clip_hash("text/plain", text.as_bytes()),
                bytes: text.as_bytes().to_vec(),
            });
        }
        assert_eq!(
            pending.recv_timeout(Duration::ZERO).unwrap().bytes,
            b"latest"
        );
        assert!(pending.recv_timeout(Duration::ZERO).is_none());
    }

    #[test]
    fn validates_incoming_clip_integrity_and_size() {
        let bytes = b"safe clipboard";
        let encoded = STANDARD.encode(bytes);
        let hash = clip_hash("text/plain", bytes);
        assert_eq!(
            decode_incoming_clip("peer", "peer", "text/plain", &hash, &encoded, 1024).unwrap(),
            bytes
        );
        assert!(decode_incoming_clip("peer", "peer", "text/plain", "bad", &encoded, 1024).is_err());
        assert!(
            decode_incoming_clip("peer", "other", "text/plain", &hash, &encoded, 1024).is_err()
        );
        assert!(decode_incoming_clip("peer", "peer", "text/plain", &hash, &encoded, 4).is_err());
    }
}
