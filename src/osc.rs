//! Async UDP I/O for OSC, using tokio sockets end to end.
//!
//! Unlike the generic bridge's std-thread plumbing, `rosc`'s encoder and
//! decoder work fine alongside tokio's async `UdpSocket` — encoding is a
//! pure byte transform, so only the socket I/O needs awaiting.

use std::net::SocketAddr;

use rosc::decoder::decode_udp;
use rosc::{encoder, OscMessage, OscPacket};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Capacity of the send and receive queues.
const CHANNEL_CAPACITY: usize = 256;

/// Spawn a task that encodes queued OSC messages and sends them to `target`.
///
/// Dropping the returned sender stops the task.
pub fn spawn_osc_sender(socket: UdpSocket, target: SocketAddr) -> mpsc::Sender<OscMessage> {
    let (tx, mut rx) = mpsc::channel::<OscMessage>(CHANNEL_CAPACITY);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match encoder::encode(&OscPacket::Message(msg)) {
                Ok(bytes) => {
                    if let Err(e) = socket.send_to(&bytes, target).await {
                        eprintln!("UDP send error: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("OSC encode error: {e}");
                }
            }
        }
    });
    tx
}

/// An OSC message received over UDP.
#[derive(Debug, Clone)]
pub struct OscReceived {
    pub message: OscMessage,
    /// Source address of the sender (useful for logging and filtering).
    pub src: SocketAddr,
}

/// Spawn a task that receives UDP packets and queues decoded OSC messages.
///
/// Bundles are logged and dropped (consistent with the generic bridge's v1
/// behaviour). Dropping the returned receiver stops the task.
pub fn spawn_osc_listener(socket: UdpSocket) -> mpsc::Receiver<OscReceived> {
    let (tx, rx) = mpsc::channel::<OscReceived>(CHANNEL_CAPACITY);
    tokio::spawn(async move {
        let mut buf = [0u8; rosc::decoder::MTU];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((size, src)) => match decode_udp(&buf[..size]) {
                    Ok((_read, OscPacket::Message(msg))) => {
                        if tx.send(OscReceived { message: msg, src }).await.is_err() {
                            // Receiver dropped — stop the task.
                            break;
                        }
                    }
                    Ok((_read, OscPacket::Bundle(bundle))) => {
                        eprintln!(
                            "Received OSC bundle from {src} ({} messages) — bundles not yet supported",
                            bundle.content.len()
                        );
                    }
                    Err(e) => {
                        eprintln!("OSC decode error from {src}: {e}");
                    }
                },
                Err(e) => {
                    // Transient errors shouldn't kill the listener.
                    eprintln!("UDP recv error: {e}");
                }
            }
        }
    });
    rx
}

/// Bind the bridge's feedback listener on localhost.
pub async fn bind_listener(port: u16) -> anyhow::Result<UdpSocket> {
    Ok(UdpSocket::bind(("127.0.0.1", port)).await?)
}

/// Bind the bridge's command sender on an ephemeral localhost port.
pub async fn bind_sender() -> anyhow::Result<UdpSocket> {
    Ok(UdpSocket::bind(("127.0.0.1", 0)).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosc::OscType;
    use std::time::Duration;
    use tokio::time::timeout;

    fn sample_message() -> OscMessage {
        OscMessage {
            addr: "/track/1/volume".to_string(),
            args: vec![OscType::Float(0.5)],
        }
    }

    #[tokio::test]
    async fn sender_delivers_encoded_osc() {
        let rx_sock = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let tx_sock = bind_sender().await.unwrap();
        let target = rx_sock.local_addr().unwrap();

        let tx = spawn_osc_sender(tx_sock, target);
        tx.send(sample_message()).await.unwrap();

        let mut buf = [0u8; rosc::decoder::MTU];
        let (size, _) = timeout(Duration::from_secs(2), rx_sock.recv_from(&mut buf))
            .await
            .expect("timed out waiting for OSC packet")
            .unwrap();
        match decode_udp(&buf[..size]).unwrap().1 {
            OscPacket::Message(msg) => {
                assert_eq!(msg.addr, "/track/1/volume");
                assert_eq!(msg.args, vec![OscType::Float(0.5)]);
            }
            other => panic!("Expected message packet, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn listener_decodes_and_reports_source() {
        let sock = bind_listener(0).await.unwrap();
        let addr = sock.local_addr().unwrap();
        let mut rx_queue = spawn_osc_listener(sock);

        let peer = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let bytes = encoder::encode(&OscPacket::Message(sample_message())).unwrap();
        peer.send_to(&bytes, addr).await.unwrap();

        let received = timeout(Duration::from_secs(2), rx_queue.recv())
            .await
            .expect("timed out waiting for OSC message")
            .unwrap();
        assert_eq!(received.message.addr, "/track/1/volume");
        assert_eq!(received.src, peer.local_addr().unwrap());
    }

    #[tokio::test]
    async fn bundles_are_logged_and_dropped() {
        let sock = bind_listener(0).await.unwrap();
        let addr = sock.local_addr().unwrap();
        let mut rx_queue = spawn_osc_listener(sock);

        let bundle = OscPacket::Bundle(rosc::OscBundle {
            // Zero timetag means "immediately" — the value is irrelevant here
            // since bundles are dropped by the listener anyway.
            timetag: rosc::OscTime {
                seconds: 0,
                fractional: 0,
            },
            content: vec![OscPacket::Message(sample_message())],
        });
        let peer = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let bytes = encoder::encode(&bundle).unwrap();
        peer.send_to(&bytes, addr).await.unwrap();

        let result = timeout(Duration::from_millis(300), rx_queue.recv()).await;
        assert!(result.is_err(), "bundle should be dropped, not delivered");
    }
}
