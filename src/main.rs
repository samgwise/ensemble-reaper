//! ensemble-reaper — REAPER OSC bridge for the Ensemble hub.
//!
//! Translates between REAPER's control-surface OSC interface and Ensemble's
//! `/reaper/**` address space:
//!
//! - REAPER feedback (transport, tracks, FX parameters) is published as
//!   Ensemble actions — state as Params (retained and replayed by the hub),
//!   the beat position as a Stream. Feedback is stamped with hub-synced time
//!   so REAPER-originated changes participate in Ensemble's timed delivery.
//! - Ensemble actions under `/reaper/**` are forwarded to REAPER as OSC
//!   commands (transport, track/FX values, action IDs).
//!
//! Two guards keep the feedback loop from spinning:
//!
//! 1. Actions the hub routes back to us carry our own `source` voice ID and
//!    are dropped (publishing feedback must not re-trigger commands).
//! 2. REAPER echoes applied commands back as feedback; the value cache
//!    pre-seeds the expected state when a command is sent and suppresses
//!    duplicate values.
//!
//! # CLI
//!
//! ```text
//! ensemble-reaper [options]
//!
//! Options:
//!   --name <name>            Voice name shown in the hub (default: reaper-bridge)
//!   --reaper-host <host>     Host running REAPER (default: 127.0.0.1)
//!   --osc-send-port <port>   REAPER's OSC listen port (default: 8000)
//!   --osc-listen-port <port> Local UDP port for REAPER feedback (default: 9000)
//!   --hub <port>             Explicit hub port (bypasses discovery)
//! ```

mod osc;
mod reaper;
mod registry;
mod translate;

use std::net::SocketAddr;

use anyhow::Result;
use ensemble_client::Hub;
use ensemble_core::protocol::*;

use reaper::ReaperMessage;
use registry::{FxRegistry, RegistryUpdate, ValueCache};
use translate::{action_to_reaper, fx_info_address, reaper_to_action_parts, ENS_ROOT};

// ---------------------------------------------------------------------------
// CLI configuration
// ---------------------------------------------------------------------------

/// Configuration parsed from CLI arguments.
struct Config {
    /// Voice name for the hub.
    name: String,
    /// Host running REAPER.
    reaper_host: String,
    /// Port to send OSC commands to (REAPER's listen port).
    osc_send_port: u16,
    /// Local UDP port to listen for REAPER feedback.
    osc_listen_port: u16,
    /// Explicit hub port (bypasses discovery).
    hub_port: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "reaper-bridge".to_string(),
            reaper_host: "127.0.0.1".to_string(),
            osc_send_port: 8000,
            osc_listen_port: 9000,
            hub_port: None,
        }
    }
}

impl Config {
    /// Parse CLI arguments into a Config.
    fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut config = Config::default();

        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--name" => {
                    if i + 1 < args.len() {
                        config.name = args[i + 1].clone();
                        i += 1;
                    }
                }
                "--reaper-host" => {
                    if i + 1 < args.len() {
                        config.reaper_host = args[i + 1].clone();
                        i += 1;
                    }
                }
                "--osc-send-port" => {
                    if i + 1 < args.len() {
                        if let Ok(port) = args[i + 1].parse() {
                            config.osc_send_port = port;
                        }
                        i += 1;
                    }
                }
                "--osc-listen-port" => {
                    if i + 1 < args.len() {
                        if let Ok(port) = args[i + 1].parse() {
                            config.osc_listen_port = port;
                        }
                        i += 1;
                    }
                }
                "--hub" => {
                    if i + 1 < args.len() {
                        if let Ok(port) = args[i + 1].parse() {
                            config.hub_port = Some(port);
                        }
                        i += 1;
                    }
                }
                other => {
                    eprintln!("Unknown argument: {other}");
                }
            }
            i += 1;
        }

        config
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// Build the voice manifest advertising the bridge's routes.
fn build_manifest(name: &str) -> VoiceManifest {
    let route = |address: &str, signal: SignalType, hint: &str, desc: &str| RouteInfo {
        address: address.to_string(),
        signal,
        payload_hint: Some(hint.to_string()),
        description: Some(desc.to_string()),
        example: None,
    };

    VoiceManifest {
        name: name.to_string(),
        description: Some(
            "REAPER OSC bridge — transport, tracks and FX parameters as Ensemble actions"
                .to_string(),
        ),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        tags: vec!["reaper".into(), "daw".into(), "osc".into()],
        provides: vec![format!("{ENS_ROOT}/**")],
        expects: vec![],
        routes: vec![
            route("/reaper/transport/play", SignalType::Event, "()", "Start transport playback"),
            route("/reaper/transport/stop", SignalType::Event, "()", "Stop the transport"),
            route("/reaper/transport/pause", SignalType::Event, "()", "Pause the transport"),
            route("/reaper/transport/rewind", SignalType::Event, "()", "Begin rewinding"),
            route("/reaper/transport/record", SignalType::Event, "()", "Toggle recording"),
            route("/reaper/transport/playing", SignalType::Param, "bool", "Transport rolling state"),
            route("/reaper/transport/recording", SignalType::Param, "bool", "Recording state"),
            route("/reaper/transport/bpm", SignalType::Param, "float", "Tempo in beats per minute"),
            route("/reaper/transport/position/seconds", SignalType::Param, "float", "Playhead position in seconds"),
            route("/reaper/transport/position/beat", SignalType::Stream, "float", "Playhead position in beats (high rate)"),
            route("/reaper/master/volume", SignalType::Param, "float 0-1", "Master fader position"),
            route("/reaper/track/{n}/volume", SignalType::Param, "float 0-1", "Track fader position"),
            route("/reaper/track/{n}/pan", SignalType::Param, "float 0-1", "Track pan (0.5 centred)"),
            route("/reaper/track/{n}/mute", SignalType::Param, "bool", "Track mute"),
            route("/reaper/track/{n}/solo", SignalType::Param, "bool", "Track solo"),
            route("/reaper/track/{n}/recarm", SignalType::Param, "bool", "Track record arm"),
            route("/reaper/track/{n}/name", SignalType::Param, "string", "Track name"),
            route("/reaper/track/{t}/fx/{f}/param/{p}/value", SignalType::Param, "float 0-1", "Normalised FX parameter value"),
            route("/reaper/track/{t}/fx/{f}/info", SignalType::Param, "map", "Plugin registry entry (name, param names)"),
            route("/reaper/action/{id}", SignalType::Event, "int", "Trigger a REAPER action by command ID"),
        ],
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_args();

    eprintln!("Ensemble REAPER bridge");
    eprintln!("  Voice name: {}", config.name);
    eprintln!(
        "  REAPER OSC target: {}:{}",
        config.reaper_host, config.osc_send_port
    );
    eprintln!(
        "  Feedback listener: 127.0.0.1:{}",
        config.osc_listen_port
    );

    // Connect to hub.
    let mut hub = if let Some(port) = config.hub_port {
        eprintln!("Connecting to hub on port {port}...");
        Hub::connect(port, &config.name).await?
    } else {
        eprintln!("Discovering hub...");
        Hub::connect_with_discovery(&config.name).await?
    };
    eprintln!("Connected to hub as voice #{}", hub.voice_id);

    // Subscribe to our whole namespace: commands from other voices arrive
    // here; our own published feedback comes back too and is filtered below
    // by source voice ID.
    hub.subscribe(&format!("{ENS_ROOT}/**")).await?;
    eprintln!("Subscribed to: {ENS_ROOT}/**");

    // Advertise capabilities.
    hub.set_manifest(&build_manifest(&config.name)).await?;

    // OSC sockets and tasks.
    let reaper_target: SocketAddr =
        format!("{}:{}", config.reaper_host, config.osc_send_port).parse()?;
    let send_socket = osc::bind_sender().await?;
    let osc_tx = osc::spawn_osc_sender(send_socket, reaper_target);
    let listen_socket = osc::bind_listener(config.osc_listen_port).await?;
    let mut osc_rx = osc::spawn_osc_listener(listen_socket);
    eprintln!(
        "UDP ready — commands → {reaper_target}, feedback on 127.0.0.1:{}",
        config.osc_listen_port
    );

    let mut registry = FxRegistry::new();
    let mut cache = ValueCache::new();
    let hub_tx = hub.sender();

    loop {
        tokio::select! {
            // ----------------------------------------------------------------
            // Hub → REAPER: forward recognised /reaper/** commands.
            // ----------------------------------------------------------------
            action_msg = hub.recv_action() => {
                let Some(action_msg) = action_msg else {
                    eprintln!("Hub connection closed.");
                    break;
                };
                // The client also forwards the hub's unset_param broadcasts
                // on this channel; only genuine actions are translatable.
                if action_msg.msg_type != MSG_ACTION {
                    continue;
                }
                let map = match &action_msg.payload {
                    Value::Map(m) => m.clone(),
                    _ => continue,
                };
                // Drop our own published feedback echoed back through the
                // hub — re-sending it would loop straight back to REAPER.
                let source = get_integer(&map, "source").unwrap_or(0);
                if source as VoiceId == hub.voice_id {
                    continue;
                }
                let address = get_string(&map, "address").unwrap_or_default();
                let payload = get_value(&map, "payload").unwrap_or(Value::Null);

                let Some(reaper_msg) = action_to_reaper(&address, &payload) else {
                    continue;
                };
                let Some(osc_msg) = reaper_msg.to_osc() else {
                    continue;
                };

                // Pre-seed the dedupe cache with the state we expect REAPER
                // to echo, so the echo is swallowed. Command-only variants
                // (transport triggers, action IDs) are skipped — their
                // feedback is genuinely new state we want to publish.
                if let Some((addr, _signal, value)) = reaper_to_action_parts(&reaper_msg) {
                    cache.seed(&addr, value);
                }

                eprintln!("→ REAPER: {} {:?}", osc_msg.addr, osc_msg.args);
                if osc_tx.send(osc_msg).await.is_err() {
                    break;
                }
            }
            // ----------------------------------------------------------------
            // REAPER → hub: publish feedback as Ensemble actions.
            // ----------------------------------------------------------------
            received = osc_rx.recv() => {
                let Some(received) = received else {
                    eprintln!("OSC listener stopped.");
                    break;
                };
                let Some(reaper_msg) = ReaperMessage::parse(&received.message) else {
                    eprintln!(
                        "  ← unmodelled OSC from {}: {}",
                        received.src, received.message.addr
                    );
                    continue;
                };

                // FX name feedback feeds the plugin registry; everything
                // else maps directly to an Ensemble action.
                let updates = match &reaper_msg {
                    ReaperMessage::FxName { track, fx, name } => {
                        registry.observe_name(*track, *fx, name)
                    }
                    ReaperMessage::FxParamName { track, fx, param, name } => {
                        registry.observe_param_name(*track, *fx, *param, name)
                    }
                    _ => {
                        let Some((addr, signal, payload)) = reaper_to_action_parts(&reaper_msg)
                        else {
                            continue;
                        };
                        // Swallow duplicates (including REAPER's command echoes).
                        if !cache.check_and_store(&addr, payload.clone()) {
                            continue;
                        }
                        let ts = hub.now().await;
                        eprintln!("  ← {}: {:?}", addr, payload);
                        let _ = hub_tx.send(action(&addr, signal, ts, payload)).await;
                        continue;
                    }
                };

                for update in updates {
                    match update {
                        RegistryUpdate::PublishInfo { track, fx, payload } => {
                            let addr = fx_info_address(track, fx);
                            if !cache.check_and_store(&addr, payload.clone()) {
                                continue;
                            }
                            let ts = hub.now().await;
                            eprintln!("  ← registry: {addr}");
                            let _ = hub_tx
                                .send(action(&addr, SignalType::Param, ts, payload))
                                .await;
                        }
                        RegistryUpdate::ClearSlot { track, fx, unset_addresses } => {
                            eprintln!(
                                "  ← registry: {} cleared",
                                fx_info_address(track, fx)
                            );
                            // Drop cached values for the whole slot so a
                            // plugin reloaded there republishes cleanly.
                            cache
                                .clear_prefix(&format!("{ENS_ROOT}/track/{track}/fx/{fx}/"));
                            for addr in unset_addresses {
                                let _ = hub_tx.send(unset_param(&addr)).await;
                            }
                        }
                    }
                }
            }
        }
    }

    hub.disconnect().await;
    eprintln!("REAPER bridge shut down.");
    Ok(())
}
