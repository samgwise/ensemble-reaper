//! ensemble-reaper — REAPER OSC bridge for the Ensemble hub.
//!
//! Translates between REAPER's control-surface OSC interface and Ensemble's
//! `/reaper/**` address space:
//!
//! - REAPER feedback (transport, tracks, FX parameters) is published as
//!   Ensemble Params, replayed by the hub to late-joining voices.
//! - Ensemble actions under `/reaper/**` are forwarded to REAPER as OSC
//!   commands (transport, track/FX values, action IDs).
//!
//! # Status
//!
//! Milestone 1: the REAPER wire model ([`reaper`]) and the translation layer
//! ([`translate`]) are in place and unit tested. Hub wiring, the FX registry
//! and UDP I/O arrive in later milestones.

// TODO(milestone-3): remove once main wires the bridge tasks and every
// variant has a runtime consumer.
#![allow(dead_code)]

mod osc;
mod reaper;
mod registry;
mod translate;

fn main() {
    eprintln!("ensemble-reaper: bridge wiring arrives in milestone 3.");
    eprintln!(
        "The REAPER wire model and translation layer are in place — see src/reaper.rs and src/translate.rs."
    );
}
