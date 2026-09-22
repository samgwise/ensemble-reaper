//! End-to-end integration test for the REAPER bridge.
//!
//! Topology:
//!
//! - a real hub, run **in-process** via `ensemble_hub::start_server` (the
//!   same entrypoint the hub binary uses);
//! - the `ensemble-reaper` bridge, spawned as a subprocess;
//! - a loopback UDP socket impersonating REAPER;
//! - a test voice connected through `ensemble-client`.
//!
//! The test asserts both relay directions (hub command → OSC, OSC feedback →
//! hub Param) and the echo-suppression behaviour.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ensemble_client::Hub;
use ensemble_core::protocol::*;
use ensemble_hub::start_server;
use rosc::decoder::decode_udp;
use rosc::{encoder, OscMessage, OscPacket, OscType};
use tokio::net::UdpSocket;
use tokio::time::{timeout, timeout_at, Instant};

/// Guard that kills the child process when dropped (std does not).
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Grab a free UDP port by binding an ephemeral socket and releasing it.
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn end_to_end_bridge_relay() {
    // Real hub, in-process, on an OS-picked port.
    let (_state, hub_port) = start_server(0).await.unwrap();

    // The test voice watches the bridge's namespace and its readiness signal.
    let mut voice = Hub::connect(hub_port, "test-voice").await.unwrap();
    voice.subscribe("/reaper/**").await.unwrap();
    // The bridge publishes its manifest only after its subscription is
    // registered and its UDP ports are bound, so this event doubles as a
    // deterministic "bridge is ready" signal.
    voice.subscribe("/hub/manifest/set").await.unwrap();

    // Fake REAPER: a plain loopback UDP socket.
    let reaper = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    let reaper_port = reaper.local_addr().unwrap().port();

    // The bridge's feedback port: grab a free port and hand it over (the
    // bridge binds it in its own process). Feedback from the fake REAPER is
    // sent to this address.
    let bridge_port = free_udp_port();

    let mut bridge = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_ensemble-reaper"))
            .args([
                "--hub",
                &hub_port.to_string(),
                "--osc-send-port",
                &reaper_port.to_string(),
                "--osc-listen-port",
                &bridge_port.to_string(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the bridge subprocess"),
    );

    // Wait until the bridge has connected, subscribed and bound its ports.
    timeout(Duration::from_secs(10), async {
        loop {
            let msg = voice.recv_action().await.expect("hub connection closed");
            let Value::Map(map) = &msg.payload else {
                continue;
            };
            // set_manifest emits /hub/manifest/set (patch_manifest emits
            // /hub/manifest/updated — the bridge uses set_manifest).
            if get_string(map, "address").as_deref() == Some("/hub/manifest/set") {
                break;
            }
        }
    })
    .await
    .expect("bridge did not become ready in time");

    let mut buf = [0u8; rosc::decoder::MTU];

    // -------------------------------------------------------------------
    // A: hub command → OSC command to REAPER.
    // -------------------------------------------------------------------
    voice
        .send_action(action(
            "/reaper/transport/play",
            SignalType::Event,
            0.0,
            Value::Bool(true),
        ))
        .await
        .unwrap();

    let (size, _) = timeout(Duration::from_secs(5), reaper.recv_from(&mut buf))
        .await
        .expect("timed out waiting for the OSC command from the bridge")
        .unwrap();
    match decode_udp(&buf[..size]).unwrap().1 {
        OscPacket::Message(m) => {
            assert_eq!(m.addr, "/play");
            assert_eq!(m.args, vec![OscType::Float(1.0)]);
        }
        other => panic!("expected an OSC message, got {other:?}"),
    }

    // -------------------------------------------------------------------
    // B: REAPER feedback → hub Param.
    // -------------------------------------------------------------------
    let feedback = OscPacket::Message(OscMessage {
        addr: "/track/3/volume".into(),
        args: vec![OscType::Float(0.75)],
    });
    let bridge_addr = format!("127.0.0.1:{bridge_port}");
    reaper
        .send_to(&encoder::encode(&feedback).unwrap(), bridge_addr.as_str())
        .await
        .unwrap();

    let msg = timeout(Duration::from_secs(5), voice.recv_action())
        .await
        .expect("timed out waiting for the bridge to publish feedback")
        .expect("hub connection closed");
    assert_eq!(msg.msg_type, MSG_ACTION);
    let Value::Map(map) = &msg.payload else {
        panic!("expected a Map payload")
    };
    assert_eq!(
        get_string(map, "address").as_deref(),
        Some("/reaper/track/3/volume")
    );
    assert_eq!(get_string(map, "signal_type").as_deref(), Some("param"));
    match get_value(map, "payload").unwrap() {
        Value::Float(f) => assert!((f.value() - 0.75).abs() < 1e-6),
        other => panic!("expected a Float payload, got {other:?}"),
    }

    // -------------------------------------------------------------------
    // C: command echoes are suppressed; genuine changes pass through.
    // -------------------------------------------------------------------
    voice
        .send_action(action(
            "/reaper/track/3/volume",
            SignalType::Param,
            0.0,
            Value::Float(0.9.into()),
        ))
        .await
        .unwrap();

    let (size, _) = timeout(Duration::from_secs(5), reaper.recv_from(&mut buf))
        .await
        .expect("timed out waiting for the volume command")
        .unwrap();
    match decode_udp(&buf[..size]).unwrap().1 {
        OscPacket::Message(m) => assert_eq!(m.addr, "/track/3/volume"),
        other => panic!("expected an OSC message, got {other:?}"),
    }

    // The fake REAPER applies the command and echoes it back...
    let echo = OscPacket::Message(OscMessage {
        addr: "/track/3/volume".into(),
        args: vec![OscType::Float(0.9)],
    });
    reaper
        .send_to(&encoder::encode(&echo).unwrap(), bridge_addr.as_str())
        .await
        .unwrap();

    // ...and the bridge must NOT republish it (pre-seeded cache).
    let deadline = Instant::now() + Duration::from_millis(700);
    loop {
        match timeout_at(deadline, voice.recv_action()).await {
            Ok(Some(msg)) => {
                let Value::Map(map) = &msg.payload else {
                    continue;
                };
                if get_string(map, "address").as_deref() == Some("/reaper/track/3/volume") {
                    panic!("echo feedback was republished: {map:?}");
                }
                // Anything else in the window is unrelated — keep waiting.
            }
            Ok(None) => panic!("hub connection closed"),
            Err(_) => break, // suppression window elapsed, nothing republished
        }
    }

    // A genuine value change must still be published.
    let changed = OscPacket::Message(OscMessage {
        addr: "/track/3/volume".into(),
        args: vec![OscType::Float(0.95)],
    });
    reaper
        .send_to(&encoder::encode(&changed).unwrap(), bridge_addr.as_str())
        .await
        .unwrap();
    let msg = timeout(Duration::from_secs(5), voice.recv_action())
        .await
        .expect("timed out waiting for the changed value")
        .expect("hub connection closed");
    let Value::Map(map) = &msg.payload else {
        panic!("expected a Map payload")
    };
    match get_value(map, "payload").unwrap() {
        Value::Float(f) => assert!((f.value() - 0.95).abs() < 1e-6),
        other => panic!("expected a Float payload, got {other:?}"),
    }

    // Explicit teardown (also covered by Drop).
    bridge.0.kill().unwrap();
    voice.disconnect().await;
}
