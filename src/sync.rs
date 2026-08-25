use anyhow::Result;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::identity::Identity;
use crate::protocol::{self, Msg, PROTO};

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

pub async fn run_session(
    conn: Connection,
    as_client: bool,
    identity: Identity,
    mut outgoing: broadcast::Receiver<OutgoingClip>,
    incoming: mpsc::Sender<IncomingClip>,
) -> Result<()> {
    let peer_addr = conn.remote_address();
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
    let (peer_id, peer_name) = match hello {
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

    info!("connected to {peer_name} ({peer_id}) at {peer_addr}");

    let mut ping = tokio::time::interval(std::time::Duration::from_secs(20));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ping.tick() => {
                if protocol::write_msg(&mut send, &Msg::Ping).await.is_err() {
                    break;
                }
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
                        if let Err(err) = protocol::write_msg(&mut send, &msg).await {
                            warn!("send to {peer_name} failed: {err}");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!("session to {peer_name} lagged {n} clips");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = protocol::read_msg(&mut recv) => {
                match msg {
                    Ok(Msg::Ping) => {
                        let _ = protocol::write_msg(&mut send, &Msg::Pong).await;
                    }
                    Ok(Msg::Pong) => {}
                    Ok(Msg::Clip { origin, mime, hash, data_b64 }) => {
                        match STANDARD.decode(data_b64) {
                            Ok(bytes) => {
                                if incoming.send(IncomingClip {
                                    from: peer_id.clone(),
                                    origin,
                                    mime,
                                    hash,
                                    bytes,
                                }).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => warn!("bad clip from {peer_name}: {err}"),
                        }
                    }
                    Ok(Msg::Hello { .. }) => {}
                    Ok(other) => debug!("ignored {other:?} from {peer_name}"),
                    Err(err) => {
                        info!("session with {peer_name} ended: {err}");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
