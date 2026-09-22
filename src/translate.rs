//! Translation between REAPER messages and Ensemble actions.
//!
//! This is the semantic heart of the bridge: it assigns each REAPER message
//! an Ensemble address under `/reaper/**` and a signal type:
//!
//! - **Param** — stateful values (transport state, track/FX values, names).
//!   Retained by the hub and replayed to late subscribers, so any voice can
//!   discover the session state without being connected when REAPER started.
//! - **Stream** — high-rate, best-effort data (the beat position).
//! - **Event** — one-shot triggers (transport commands, action IDs).

use ensemble_core::protocol::{SignalType, Value};

use crate::reaper::{ReaperMessage, TransportCommand};

/// The Ensemble namespace root for all REAPER addresses.
pub const ENS_ROOT: &str = "/reaper";

/// Address of an FX slot's `info` Param (the plugin registry entry).
pub fn fx_info_address(track: u32, fx: u32) -> String {
    format!("{ENS_ROOT}/track/{track}/fx/{fx}/info")
}

/// Address of an FX parameter's value Param.
pub fn fx_param_value_address(track: u32, fx: u32, param: u32) -> String {
    format!("{ENS_ROOT}/track/{track}/fx/{fx}/param/{param}/value")
}

/// Convert REAPER feedback into the components of an Ensemble action:
/// the address, the signal type and the payload. The caller supplies the
/// timestamp at the send site (hub-synced time).
///
/// Returns `None` for command-only variants (transport commands, action IDs)
/// which have no REAPER-side feedback representation.
pub fn reaper_to_action_parts(msg: &ReaperMessage) -> Option<(String, SignalType, Value)> {
    let mut address = String::from(ENS_ROOT);
    let (signal, payload) = match msg {
        ReaperMessage::Playing(on) => {
            address.push_str("/transport/playing");
            (SignalType::Param, Value::Bool(*on))
        }
        ReaperMessage::Recording(on) => {
            address.push_str("/transport/recording");
            (SignalType::Param, Value::Bool(*on))
        }
        ReaperMessage::Tempo(bpm) => {
            address.push_str("/transport/bpm");
            (SignalType::Param, Value::Float((*bpm).into()))
        }
        ReaperMessage::PositionSeconds(s) => {
            address.push_str("/transport/position/seconds");
            (SignalType::Param, Value::Float((*s).into()))
        }
        // The beat position ticks continuously while playing — best-effort
        // delivery keeps it from flooding the hub's retained state.
        ReaperMessage::PositionBeat(b) => {
            address.push_str("/transport/position/beat");
            (SignalType::Stream, Value::Float((*b).into()))
        }
        ReaperMessage::TrackVolume { track, value } => {
            address.push_str(&format!("/track/{track}/volume"));
            (SignalType::Param, Value::Float((*value).into()))
        }
        ReaperMessage::TrackPan { track, value } => {
            address.push_str(&format!("/track/{track}/pan"));
            (SignalType::Param, Value::Float((*value).into()))
        }
        ReaperMessage::TrackMute { track, on } => {
            address.push_str(&format!("/track/{track}/mute"));
            (SignalType::Param, Value::Bool(*on))
        }
        ReaperMessage::TrackSolo { track, on } => {
            address.push_str(&format!("/track/{track}/solo"));
            (SignalType::Param, Value::Bool(*on))
        }
        ReaperMessage::TrackRecArm { track, on } => {
            address.push_str(&format!("/track/{track}/recarm"));
            (SignalType::Param, Value::Bool(*on))
        }
        ReaperMessage::TrackName { track, name } => {
            address.push_str(&format!("/track/{track}/name"));
            (SignalType::Param, Value::String(name.clone()))
        }
        ReaperMessage::MasterVolume(value) => {
            address.push_str("/master/volume");
            (SignalType::Param, Value::Float((*value).into()))
        }
        ReaperMessage::FxName { track, fx, name } => {
            address.push_str(&format!("/track/{track}/fx/{fx}/name"));
            (SignalType::Param, Value::String(name.clone()))
        }
        ReaperMessage::FxBypass { track, fx, on } => {
            address.push_str(&format!("/track/{track}/fx/{fx}/bypass"));
            (SignalType::Param, Value::Bool(*on))
        }
        ReaperMessage::FxParamName {
            track,
            fx,
            param,
            name,
        } => {
            address.push_str(&format!("/track/{track}/fx/{fx}/param/{param}/name"));
            (SignalType::Param, Value::String(name.clone()))
        }
        ReaperMessage::FxParamValue {
            track,
            fx,
            param,
            value,
        } => {
            address.push_str(&format!("/track/{track}/fx/{fx}/param/{param}/value"));
            (SignalType::Param, Value::Float((*value).into()))
        }
        // Command-only variants are sent to REAPER, never published.
        ReaperMessage::Transport(_) | ReaperMessage::Action(_) => return None,
    };
    Some((address, signal, payload))
}

/// Convert an Ensemble action (address + payload) into a REAPER message.
///
/// Returns `None` for addresses outside the REAPER namespace and for
/// read-only addresses (position, names) that cannot be commanded.
pub fn action_to_reaper(address: &str, payload: &Value) -> Option<ReaperMessage> {
    let rest = strip_prefix_segment(address, ENS_ROOT)?;
    let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let msg = match segs.as_slice() {
        ["transport", "play"] => ReaperMessage::Transport(TransportCommand::Play),
        ["transport", "pause"] => ReaperMessage::Transport(TransportCommand::Pause),
        ["transport", "stop"] => ReaperMessage::Transport(TransportCommand::Stop),
        ["transport", "rewind"] => ReaperMessage::Transport(TransportCommand::Rewind),
        ["transport", "record"] => ReaperMessage::Transport(TransportCommand::Record),
        ["transport", "bpm"] => ReaperMessage::Tempo(value_f64(payload)?),
        ["master", "volume"] => ReaperMessage::MasterVolume(value_f64(payload)?),
        ["track", t, "volume"] => ReaperMessage::TrackVolume {
            track: seg_index(t)?,
            value: value_f64(payload)?,
        },
        ["track", t, "pan"] => ReaperMessage::TrackPan {
            track: seg_index(t)?,
            value: value_f64(payload)?,
        },
        ["track", t, "mute"] => ReaperMessage::TrackMute {
            track: seg_index(t)?,
            on: value_bool(payload)?,
        },
        ["track", t, "solo"] => ReaperMessage::TrackSolo {
            track: seg_index(t)?,
            on: value_bool(payload)?,
        },
        ["track", t, "recarm"] => ReaperMessage::TrackRecArm {
            track: seg_index(t)?,
            on: value_bool(payload)?,
        },
        ["track", t, "name"] => ReaperMessage::TrackName {
            track: seg_index(t)?,
            name: value_string(payload)?.to_owned(),
        },
        ["track", t, "fx", f, "bypass"] => ReaperMessage::FxBypass {
            track: seg_index(t)?,
            fx: seg_index(f)?,
            on: value_bool(payload)?,
        },
        ["track", t, "fx", f, "param", p, "value"] => ReaperMessage::FxParamValue {
            track: seg_index(t)?,
            fx: seg_index(f)?,
            param: seg_index(p)?,
            value: value_f64(payload)?,
        },
        ["action", id] => ReaperMessage::Action(seg_index(id)?),
        // Read-only addresses (position, playing/recording state, names)
        // and unknown addresses have no command representation.
        _ => return None,
    };
    Some(msg)
}

/// Strip `prefix` from `addr` on a segment boundary.
///
/// Returns `Some(rest)` only when `addr` equals the prefix exactly or
/// continues with a `/`-delimited segment — so `/reaperx/…` does not match
/// the `/reaper` root.
fn strip_prefix_segment<'a>(addr: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = addr.strip_prefix(prefix)?;
    if rest.is_empty() || rest.starts_with('/') {
        Some(rest)
    } else {
        None
    }
}

fn seg_index(seg: &str) -> Option<u32> {
    seg.parse().ok()
}

fn value_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(f.value()),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

fn value_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Integer(i) => Some(*i != 0),
        Value::Float(f) => Some(f.value() != 0.0),
        _ => None,
    }
}

fn value_string(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_state_becomes_param() {
        let (addr, signal, payload) = reaper_to_action_parts(&ReaperMessage::Playing(true)).unwrap();
        assert_eq!(addr, "/reaper/transport/playing");
        assert_eq!(signal, SignalType::Param);
        assert_eq!(payload, Value::Bool(true));
    }

    #[test]
    fn beat_position_becomes_stream() {
        let (addr, signal, _) = reaper_to_action_parts(&ReaperMessage::PositionBeat(8.5)).unwrap();
        assert_eq!(addr, "/reaper/transport/position/beat");
        assert_eq!(signal, SignalType::Stream);
    }

    #[test]
    fn fx_param_value_address_mapping() {
        let msg = ReaperMessage::FxParamValue { track: 3, fx: 1, param: 6, value: 0.25 };
        let (addr, signal, payload) = reaper_to_action_parts(&msg).unwrap();
        assert_eq!(addr, "/reaper/track/3/fx/1/param/6/value");
        assert_eq!(signal, SignalType::Param);
        assert_eq!(payload, Value::Float(0.25.into()));
    }

    #[test]
    fn command_variants_are_not_published() {
        assert!(reaper_to_action_parts(&ReaperMessage::Transport(TransportCommand::Play)).is_none());
        assert!(reaper_to_action_parts(&ReaperMessage::Action(40757)).is_none());
    }

    #[test]
    fn command_address_roundtrip() {
        // Ensemble → REAPER → Ensemble-style check for an FX param command.
        let msg = action_to_reaper(
            "/reaper/track/3/fx/1/param/6/value",
            &Value::Float(0.42.into()),
        )
        .unwrap();
        assert_eq!(
            msg,
            ReaperMessage::FxParamValue { track: 3, fx: 1, param: 6, value: 0.42 }
        );
        let osc = msg.to_osc().unwrap();
        assert_eq!(osc.addr, "/track/3/fx/1/fxparam/6/value");
    }

    #[test]
    fn transport_command_mapping() {
        assert_eq!(
            action_to_reaper("/reaper/transport/play", &Value::Bool(true)).unwrap(),
            ReaperMessage::Transport(TransportCommand::Play)
        );
        // Bare command (no payload semantics needed).
        assert!(action_to_reaper("/reaper/transport/stop", &Value::Null).is_some());
    }

    #[test]
    fn bool_payloads_accept_numbers() {
        // REAPER-style 1.0/0.0 floats must also work as bool payloads.
        assert_eq!(
            action_to_reaper("/reaper/track/2/mute", &Value::Float(1.0.into())).unwrap(),
            ReaperMessage::TrackMute { track: 2, on: true }
        );
    }

    #[test]
    fn rejects_out_of_namespace_and_read_only_addresses() {
        // Segment boundary: /reaperx is not under /reaper.
        assert!(action_to_reaper("/reaperx/track/1/volume", &Value::Float(0.5.into())).is_none());
        // Position is read-only.
        assert!(
            action_to_reaper("/reaper/transport/position/seconds", &Value::Float(1.0.into()))
                .is_none()
        );
        // Unknown address.
        assert!(action_to_reaper("/reaper/who/knows", &Value::Null).is_none());
    }

    #[test]
    fn action_id_from_address() {
        assert_eq!(
            action_to_reaper("/reaper/action/40757", &Value::Null).unwrap(),
            ReaperMessage::Action(40757)
        );
    }
}
