use pastebridge::crypto::clip_hash;
use pastebridge::sync::{self, IncomingClipboard, OutgoingClip, SessionOptions};
use pastebridge::{tls, Identity};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

#[tokio::test]
async fn sessions_wait_for_hello_and_enforce_text_only_reception() {
    tokio::time::timeout(Duration::from_secs(10), exercise_sessions())
        .await
        .unwrap();
}

async fn exercise_sessions() {
    pastebridge::init_crypto();
    let client_id = Identity::generate("client").unwrap();
    let server_id = Identity::generate("server").unwrap();
    let server_config = tls::server_config(
        &server_id,
        tls::PinStore::new(vec![pastebridge::crypto::cert_sha256(
            client_id.cert_der().unwrap().as_ref(),
        )]),
        true,
    )
    .unwrap();
    let server = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(
        tls::client_config(
            &client_id,
            tls::PinStore::new(vec![pastebridge::crypto::cert_sha256(
                server_id.cert_der().unwrap().as_ref(),
            )]),
            false,
        )
        .unwrap(),
    );
    let dial = client
        .connect(server.local_addr().unwrap(), "pastebridge")
        .unwrap();
    let (client_conn, server_conn) =
        tokio::join!(dial, async { server.accept().await.unwrap().await });
    let client_conn = client_conn.unwrap();
    let server_conn = server_conn.unwrap();
    let (client_out, _) = broadcast::channel(4);
    let (server_out, _) = broadcast::channel(4);
    let received = IncomingClipboard::default();
    let client_ready = Arc::new(AtomicBool::new(false));
    let server_ready = Arc::new(AtomicBool::new(false));
    let client_task = tokio::spawn(sync::run_session(
        client_conn.clone(),
        true,
        client_id.clone(),
        client_out.subscribe(),
        IncomingClipboard::default(),
        SessionOptions {
            max_payload_bytes: 1024,
            sync_images: true,
        },
        client_ready.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!client_ready.load(Ordering::Acquire));
    let server_task = tokio::spawn(sync::run_session(
        server_conn.clone(),
        false,
        server_id,
        server_out.subscribe(),
        received.clone(),
        SessionOptions {
            max_payload_bytes: 1024,
            sync_images: false,
        },
        server_ready.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while !client_ready.load(Ordering::Acquire) || !server_ready.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let send_clip = |mime: &str, bytes: &[u8]| {
        client_out
            .send(OutgoingClip {
                origin: client_id.device_id.clone(),
                mime: mime.into(),
                hash: clip_hash(mime, bytes),
                bytes: bytes.to_vec(),
            })
            .unwrap();
    };
    send_clip("image/png", b"image payload");
    let pending = received.clone();
    assert!(
        tokio::task::spawn_blocking(move || pending.recv_timeout(Duration::from_millis(150)))
            .await
            .unwrap()
            .is_none()
    );
    send_clip("text/plain", b"hello from the other computer");
    let pending = received.clone();
    let clip = tokio::task::spawn_blocking(move || pending.recv_timeout(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(clip.mime, "text/plain");
    assert_eq!(clip.bytes, b"hello from the other computer");
    assert_eq!(clip.from, client_id.device_id);
    client_conn.close(0u32.into(), b"test complete");
    server_conn.close(0u32.into(), b"test complete");
    let _ = client_task.await;
    let _ = server_task.await;
}
