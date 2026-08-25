use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const PROTO: u32 = 1;
pub const MAX_MSG: usize = 12 * 1024 * 1024;
pub const DEFAULT_PORT: u16 = 27419;
pub const PAIR_PORT: u16 = 27420;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Msg {
    Hello {
        device_id: String,
        name: String,
        proto: u32,
    },
    PairConfirm,
    PairAbort,
    Clip {
        origin: String,
        mime: String,
        hash: String,
        data_b64: String,
    },
    Ping,
    Pong,
}

pub async fn write_msg<W: AsyncWriteExt + Unpin>(writer: &mut W, msg: &Msg) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    if bytes.len() > MAX_MSG {
        bail!("message too large ({} bytes)", bytes.len());
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_msg<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Msg> {
    let len = reader.read_u32().await.context("connection closed")? as usize;
    if len > MAX_MSG {
        bail!("incoming message too large ({len} bytes)");
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).context("invalid message json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip() {
        let msg = Msg::Hello {
            device_id: "abc".into(),
            name: "box".into(),
            proto: 1,
        };
        let mut buf = Vec::new();
        write_msg(&mut buf, &msg).await.unwrap();
        let parsed = read_msg(&mut buf.as_slice()).await.unwrap();
        match parsed {
            Msg::Hello {
                device_id,
                name,
                proto,
            } => {
                assert_eq!(device_id, "abc");
                assert_eq!(name, "box");
                assert_eq!(proto, 1);
            }
            _ => panic!("wrong variant"),
        }
    }
}
