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
    MessageReader::default().read(reader).await
}

#[derive(Default)]
pub struct MessageReader {
    header: [u8; 4],
    header_read: usize,
    body: Vec<u8>,
    body_read: usize,
}

impl MessageReader {
    pub async fn read<R: AsyncReadExt + Unpin>(&mut self, reader: &mut R) -> Result<Msg> {
        while self.header_read < self.header.len() {
            let n = reader.read(&mut self.header[self.header_read..]).await?;
            if n == 0 {
                bail!("connection closed during message header");
            }
            self.header_read += n;
        }
        let len = u32::from_be_bytes(self.header) as usize;
        if len > MAX_MSG {
            bail!("incoming message too large ({len} bytes)");
        }
        self.body.resize(len, 0);
        while self.body_read < len {
            let n = reader.read(&mut self.body[self.body_read..]).await?;
            if n == 0 {
                bail!("connection closed during message body");
            }
            self.body_read += n;
        }
        self.header_read = 0;
        self.body_read = 0;
        serde_json::from_slice(&std::mem::take(&mut self.body)).context("invalid message json")
    }
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

    #[tokio::test]
    async fn interrupted_frame_resumes_without_losing_bytes() {
        let (mut sender, mut receiver) = tokio::io::duplex(128);
        let mut frame = Vec::new();
        write_msg(&mut frame, &Msg::Ping).await.unwrap();
        let mut reader = MessageReader::default();
        for split in [2, 6] {
            let start = if split == 2 { 0 } else { 2 };
            sender.write_all(&frame[start..split]).await.unwrap();
            assert!(tokio::time::timeout(
                std::time::Duration::from_millis(10),
                reader.read(&mut receiver),
            )
            .await
            .is_err());
        }
        sender.write_all(&frame[6..]).await.unwrap();
        assert!(matches!(
            reader.read(&mut receiver).await.unwrap(),
            Msg::Ping
        ));
        write_msg(&mut sender, &Msg::Pong).await.unwrap();
        assert!(matches!(
            reader.read(&mut receiver).await.unwrap(),
            Msg::Pong
        ));
    }
}
