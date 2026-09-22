//! REAPER wire model — typed representation of OSC messages exchanged with
//! REAPER's control surface OSC interface.
//!
//! Addresses follow the `Ensemble.ReaperOSC` pattern config shipped with this
//! crate (see `reaper-osc/`). Track, FX and parameter indices are passed
//! through to REAPER verbatim — the bridge never re-bases them.

use rosc::{OscMessage, OscType};

/// Transport commands the bridge can send to REAPER.
///
/// These map to REAPER's `t/` toggle patterns, which trigger on an argument
/// of 1 (or a bare message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportCommand {
    Play,
    Pause,
    Stop,
    Rewind,
    Record,
}

/// A typed REAPER OSC message.
///
/// State variants (tracks, FX parameters, tempo) are bidirectional: REAPER
/// accepts the same patterns to set values as it sends for feedback. The
/// remaining variants are feedback-only (parsed from REAPER) or
/// command-only (built for sending), as noted.
#[derive(Debug, Clone, PartialEq)]
pub enum ReaperMessage {
    // — Transport state (feedback from REAPER) —
    /// REAPER's `t/play` feedback: 1 while the transport is rolling.
    Playing(bool),
    /// REAPER's `t/record` feedback.
    Recording(bool),
    /// REAPER's `f/tempo/raw` feedback, in beats per minute. Also usable as
    /// a command to set the tempo.
    Tempo(f64),
    /// Playhead position in seconds (feedback only).
    PositionSeconds(f64),
    /// Playhead position in beats (feedback only).
    PositionBeat(f64),
    /// A transport command (sent to REAPER only).
    Transport(TransportCommand),
    // — Track state (bidirectional) —
    TrackVolume { track: u32, value: f64 },
    TrackPan { track: u32, value: f64 },
    TrackMute { track: u32, on: bool },
    TrackSolo { track: u32, on: bool },
    TrackRecArm { track: u32, on: bool },
    /// Track name; as a command this renames the track.
    TrackName { track: u32, name: String },
    MasterVolume(f64),
    // — FX state —
    /// Plugin name at an FX slot (feedback only). Consumed by the FX registry.
    FxName { track: u32, fx: u32, name: String },
    FxBypass { track: u32, fx: u32, on: bool },
    /// Parameter name (feedback only). Consumed by the FX registry.
    FxParamName { track: u32, fx: u32, param: u32, name: String },
    /// Normalised (0–1) FX parameter value, bidirectional.
    FxParamValue { track: u32, fx: u32, param: u32, value: f64 },
    /// Trigger any REAPER action by its command ID (sent to REAPER only).
    Action(u32),
}

impl ReaperMessage {
    /// Parse an inbound OSC message from REAPER into a typed message.
    ///
    /// Returns `None` for addresses the bridge does not model; the caller can
    /// log those with the raw address for debugging.
    pub fn parse(msg: &OscMessage) -> Option<Self> {
        let segs: Vec<&str> = msg.addr.split('/').filter(|s| !s.is_empty()).collect();
        let parsed = match segs.as_slice() {
            ["play"] => ReaperMessage::Playing(arg_bool(&msg.args)?),
            ["record"] => ReaperMessage::Recording(arg_bool(&msg.args)?),
            ["tempo", "raw"] => ReaperMessage::Tempo(arg_f64(&msg.args)?),
            ["position", "seconds"] => ReaperMessage::PositionSeconds(arg_f64(&msg.args)?),
            ["position", "beat"] => ReaperMessage::PositionBeat(arg_f64(&msg.args)?),
            ["master", "volume"] => ReaperMessage::MasterVolume(arg_f64(&msg.args)?),
            // ACTION accepts both `i/action` (int argument) and `t/action/@` (id in address).
            ["action"] => ReaperMessage::Action(arg_u32(&msg.args)?),
            ["action", id] => ReaperMessage::Action(seg_index(id)?),
            ["track", t, "volume"] => ReaperMessage::TrackVolume {
                track: seg_index(t)?,
                value: arg_f64(&msg.args)?,
            },
            ["track", t, "pan"] => ReaperMessage::TrackPan {
                track: seg_index(t)?,
                value: arg_f64(&msg.args)?,
            },
            ["track", t, "mute"] => ReaperMessage::TrackMute {
                track: seg_index(t)?,
                on: arg_bool(&msg.args)?,
            },
            ["track", t, "solo"] => ReaperMessage::TrackSolo {
                track: seg_index(t)?,
                on: arg_bool(&msg.args)?,
            },
            ["track", t, "recarm"] => ReaperMessage::TrackRecArm {
                track: seg_index(t)?,
                on: arg_bool(&msg.args)?,
            },
            ["track", t, "name"] => ReaperMessage::TrackName {
                track: seg_index(t)?,
                name: arg_string(&msg.args)?.to_owned(),
            },
            ["track", t, "fx", f, "name"] => ReaperMessage::FxName {
                track: seg_index(t)?,
                fx: seg_index(f)?,
                name: arg_string(&msg.args)?.to_owned(),
            },
            ["track", t, "fx", f, "bypass"] => ReaperMessage::FxBypass {
                track: seg_index(t)?,
                fx: seg_index(f)?,
                on: arg_bool(&msg.args)?,
            },
            ["track", t, "fx", f, "fxparam", p, "name"] => ReaperMessage::FxParamName {
                track: seg_index(t)?,
                fx: seg_index(f)?,
                param: seg_index(p)?,
                name: arg_string(&msg.args)?.to_owned(),
            },
            ["track", t, "fx", f, "fxparam", p, "value"] => ReaperMessage::FxParamValue {
                track: seg_index(t)?,
                fx: seg_index(f)?,
                param: seg_index(p)?,
                value: arg_f64(&msg.args)?,
            },
            _ => return None,
        };
        Some(parsed)
    }

    /// Build the OSC message to send to REAPER.
    ///
    /// Returns `None` for feedback-only variants that make no sense as
    /// commands (transport/position state, FX and parameter names).
    pub fn to_osc(&self) -> Option<OscMessage> {
        let msg = match self {
            ReaperMessage::Transport(cmd) => {
                let addr = match cmd {
                    TransportCommand::Play => "/play",
                    TransportCommand::Pause => "/pause",
                    TransportCommand::Stop => "/stop",
                    TransportCommand::Rewind => "/rewind",
                    TransportCommand::Record => "/record",
                };
                // `t/` patterns trigger on an argument of 1.
                osc(addr, vec![osc_float(1.0)])
            }
            ReaperMessage::Tempo(bpm) => osc("/tempo/raw", vec![osc_float(*bpm)]),
            ReaperMessage::MasterVolume(value) => {
                osc("/master/volume", vec![osc_float(*value)])
            }
            ReaperMessage::TrackVolume { track, value } => {
                osc(format!("/track/{track}/volume"), vec![osc_float(*value)])
            }
            ReaperMessage::TrackPan { track, value } => {
                osc(format!("/track/{track}/pan"), vec![osc_float(*value)])
            }
            ReaperMessage::TrackMute { track, on } => osc(
                format!("/track/{track}/mute"),
                vec![osc_float(bool_arg(*on))],
            ),
            ReaperMessage::TrackSolo { track, on } => osc(
                format!("/track/{track}/solo"),
                vec![osc_float(bool_arg(*on))],
            ),
            ReaperMessage::TrackRecArm { track, on } => osc(
                format!("/track/{track}/recarm"),
                vec![osc_float(bool_arg(*on))],
            ),
            ReaperMessage::TrackName { track, name } => osc(
                format!("/track/{track}/name"),
                vec![OscType::String(name.clone())],
            ),
            ReaperMessage::FxBypass { track, fx, on } => osc(
                format!("/track/{track}/fx/{fx}/bypass"),
                vec![osc_float(bool_arg(*on))],
            ),
            ReaperMessage::FxParamValue {
                track,
                fx,
                param,
                value,
            } => osc(
                format!("/track/{track}/fx/{fx}/fxparam/{param}/value"),
                vec![osc_float(*value)],
            ),
            ReaperMessage::Action(id) => osc(format!("/action/{id}"), vec![]),
            // Feedback-only variants have no command representation.
            ReaperMessage::Playing(_)
            | ReaperMessage::Recording(_)
            | ReaperMessage::PositionSeconds(_)
            | ReaperMessage::PositionBeat(_)
            | ReaperMessage::FxName { .. }
            | ReaperMessage::FxParamName { .. } => return None,
        };
        Some(msg)
    }
}

fn osc(addr: impl Into<String>, args: Vec<OscType>) -> OscMessage {
    OscMessage {
        addr: addr.into(),
        args,
    }
}

fn osc_float(value: f64) -> OscType {
    OscType::Float(value as f32)
}

fn bool_arg(on: bool) -> f64 {
    if on { 1.0 } else { 0.0 }
}

fn arg_f64(args: &[OscType]) -> Option<f64> {
    match args.first()? {
        OscType::Float(f) => Some(*f as f64),
        OscType::Int(i) => Some(*i as f64),
        _ => None,
    }
}

fn arg_bool(args: &[OscType]) -> Option<bool> {
    match args.first()? {
        OscType::Bool(b) => Some(*b),
        OscType::Float(f) => Some(*f != 0.0),
        OscType::Int(i) => Some(*i != 0),
        _ => None,
    }
}

fn arg_u32(args: &[OscType]) -> Option<u32> {
    match args.first()? {
        OscType::Int(i) => u32::try_from(*i).ok(),
        OscType::Float(f) if *f >= 0.0 && f.fract() == 0.0 => Some(*f as u32),
        _ => None,
    }
}

fn arg_string(args: &[OscType]) -> Option<&str> {
    match args.first()? {
        OscType::String(s) => Some(s),
        _ => None,
    }
}

fn seg_index(seg: &str) -> Option<u32> {
    seg.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(addr: &str, args: Vec<OscType>) -> Option<ReaperMessage> {
        ReaperMessage::parse(&osc(addr, args))
    }

    // Test float values are chosen to be exactly representable in f32,
    // since REAPER sends f32 on the wire and parse widens to f64.

    #[test]
    fn parse_transport_feedback() {
        assert_eq!(parse("/play", vec![osc_float(1.0)]), Some(ReaperMessage::Playing(true)));
        assert_eq!(parse("/play", vec![osc_float(0.0)]), Some(ReaperMessage::Playing(false)));
        assert_eq!(
            parse("/record", vec![osc_float(1.0)]),
            Some(ReaperMessage::Recording(true))
        );
    }

    #[test]
    fn parse_tempo_and_position() {
        assert_eq!(parse("/tempo/raw", vec![osc_float(120.0)]), Some(ReaperMessage::Tempo(120.0)));
        assert_eq!(
            parse("/position/seconds", vec![osc_float(42.5)]),
            Some(ReaperMessage::PositionSeconds(42.5))
        );
    }

    // 0.75, 0.25 and 0.5 are exact in f32; 0.8 and 0.42 are not.

    #[test]
    fn parse_track_state() {
        assert_eq!(
            parse("/track/3/volume", vec![osc_float(0.75)]),
            Some(ReaperMessage::TrackVolume { track: 3, value: 0.75 })
        );
        assert_eq!(
            parse("/track/1/mute", vec![osc_float(1.0)]),
            Some(ReaperMessage::TrackMute { track: 1, on: true })
        );
    }

    #[test]
    fn parse_fx_state() {
        assert_eq!(
            parse(
                "/track/2/fx/1/name",
                vec![OscType::String("VST3: ReaEQ".into())]
            ),
            Some(ReaperMessage::FxName {
                track: 2,
                fx: 1,
                name: "VST3: ReaEQ".into()
            })
        );
        assert_eq!(
            parse("/track/2/fx/1/fxparam/6/value", vec![osc_float(0.25)]),
            Some(ReaperMessage::FxParamValue { track: 2, fx: 1, param: 6, value: 0.25 })
        );
    }

    #[test]
    fn build_transport_commands() {
        let out = ReaperMessage::Transport(TransportCommand::Play).to_osc().unwrap();
        assert_eq!(out.addr, "/play");
    }

    #[test]
    fn build_track_and_fx_commands() {
        let out = ReaperMessage::TrackVolume { track: 3, value: 0.5 }.to_osc().unwrap();
        assert_eq!(out.addr, "/track/3/volume");
    }

    #[test]
    fn feedback_only_variants_have_no_command_form() {
        assert!(ReaperMessage::Playing(true).to_osc().is_none());
        assert!(ReaperMessage::FxName { track: 1, fx: 0, name: "x".into() }.to_osc().is_none());
    }
}
