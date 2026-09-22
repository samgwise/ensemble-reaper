//! FX registry and published-value cache.
//!
//! REAPER's OSC interface sends no explicit add/remove events for FX — a
//! chain change only shows up as name feedback moving between slots. The
//! registry diffs incoming `FX_NAME`/`FX_PARAM_NAME` feedback against its
//! cached state to synthesise publish/clear updates for the hub:
//!
//! - A name arriving at a previously unknown slot publishes the slot's
//!   `info` Param.
//! - A changed name republishes `info` (the old plugin's parameter indices
//!   are meaningless and dropped).
//! - An empty name clears the slot; the update carries the addresses to
//!   `unset_param` on the hub.
//!
//! The value cache is the bridge's second feedback-loop guard: REAPER echoes
//! applied commands back as feedback, and the cache swallows those echoes so
//! only genuine changes reach the hub.

use std::collections::{BTreeMap, HashMap};

use ensemble_core::protocol::Value;

use crate::translate::{fx_info_address, fx_param_value_address};

/// An update the registry wants published to the hub.
#[derive(Debug, Clone, PartialEq)]
pub enum RegistryUpdate {
    /// Publish (or republish) the `info` Param for an FX slot.
    PublishInfo { track: u32, fx: u32, payload: Value },
    /// An FX slot emptied. `unset_addresses` lists the hub addresses that
    /// should receive `unset_param` — the slot's `info` plus every known
    /// parameter value address (best-effort; unsetting an address with no
    /// retained state is harmless).
    ClearSlot {
        track: u32,
        fx: u32,
        unset_addresses: Vec<String>,
    },
}

/// Everything known about one FX slot.
#[derive(Debug, Clone, PartialEq)]
pub struct FxEntry {
    /// Plugin name as reported by REAPER (e.g. `"VST3: ReaEQ (Cockos)"`).
    pub name: String,
    /// Parameter names by index, from `FX_PARAM_NAME` feedback.
    pub params: BTreeMap<u32, String>,
}

impl FxEntry {
    /// Build the `info` Map payload for this slot.
    ///
    /// `param_count` is the highest observed parameter index + 1 — REAPER's
    /// OSC interface does not expose a plugin's true parameter count, only
    /// names for parameters as they are banked through the device window.
    fn to_value(&self) -> Value {
        let params: BTreeMap<String, Value> = self
            .params
            .iter()
            .map(|(i, n)| (i.to_string(), Value::String(n.clone())))
            .collect();
        Value::Map(BTreeMap::from([
            ("name".to_string(), Value::String(self.name.clone())),
            (
                "param_count".to_string(),
                Value::Integer(self.params.keys().next_back().map_or(0, |m| *m as i64 + 1)),
            ),
            ("params".to_string(), Value::Map(params)),
        ]))
    }

    /// Hub addresses to unset when this slot is cleared.
    fn unset_addresses(&self, track: u32, fx: u32) -> Vec<String> {
        let mut addrs = vec![fx_info_address(track, fx)];
        for p in 0..self.params.keys().next_back().map_or(0, |m| *m + 1) {
            addrs.push(fx_param_value_address(track, fx, p));
        }
        addrs
    }
}

/// Cached FX state by (track, fx) slot, diffed against incoming feedback.
#[derive(Default)]
pub struct FxRegistry {
    slots: HashMap<(u32, u32), FxEntry>,
}

impl FxRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe `FX_NAME` feedback for a slot and diff against the cache.
    ///
    /// An empty name clears the slot (REAPER reports emptied slots with an
    /// empty name in its feedback for the affected track).
    pub fn observe_name(&mut self, track: u32, fx: u32, name: &str) -> Vec<RegistryUpdate> {
        if name.is_empty() {
            return match self.slots.remove(&(track, fx)) {
                Some(entry) => vec![RegistryUpdate::ClearSlot {
                    track,
                    fx,
                    unset_addresses: entry.unset_addresses(track, fx),
                }],
                None => vec![],
            };
        }

        match self.slots.get_mut(&(track, fx)) {
            Some(entry) if entry.name == name => vec![],
            Some(entry) => {
                // Replacement plugin: parameter indices from the old plugin
                // carry no meaning for the new one.
                entry.name = name.to_owned();
                entry.params.clear();
                vec![RegistryUpdate::PublishInfo {
                    track,
                    fx,
                    payload: entry.to_value(),
                }]
            }
            None => {
                let entry = FxEntry {
                    name: name.to_owned(),
                    params: BTreeMap::new(),
                };
                let payload = entry.to_value();
                self.slots.insert((track, fx), entry);
                vec![RegistryUpdate::PublishInfo {
                    track,
                    fx,
                    payload,
                }]
            }
        }
    }

    /// Observe `FX_PARAM_NAME` feedback for a slot.
    ///
    /// Parameter names for an unknown slot are ignored — a loaded FX always
    /// announces its name first, and a nameless entry cannot be published.
    pub fn observe_param_name(
        &mut self,
        track: u32,
        fx: u32,
        param: u32,
        name: &str,
    ) -> Vec<RegistryUpdate> {
        let Some(entry) = self.slots.get_mut(&(track, fx)) else {
            return vec![];
        };
        if entry.params.get(&param).map(String::as_str) == Some(name) {
            return vec![];
        }
        entry.params.insert(param, name.to_owned());
        vec![RegistryUpdate::PublishInfo {
            track,
            fx,
            payload: entry.to_value(),
        }]
    }

    /// Inspect a slot (used by tests and diagnostics).
    pub fn entry(&self, track: u32, fx: u32) -> Option<&FxEntry> {
        self.slots.get(&(track, fx))
    }
}

/// Dedupe cache for values published to the hub.
///
/// Keyed by Ensemble address; floats compare by bit pattern (via
/// `FloatValue`), so exact wire-precision echoes are swallowed reliably.
#[derive(Default)]
pub struct ValueCache {
    last: HashMap<String, Value>,
}

impl ValueCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `value` for `address` and report whether it differs from the
    /// last value seen there — `true` means publish, `false` means duplicate.
    pub fn check_and_store(&mut self, address: &str, value: Value) -> bool {
        if self.last.get(address) == Some(&value) {
            return false;
        }
        self.last.insert(address.to_owned(), value);
        true
    }

    /// Record a value as already sent without publishing it.
    ///
    /// Used when the bridge sends a command to REAPER: pre-seeding the cache
    /// swallows the echo feedback REAPER sends back once it applies.
    pub fn seed(&mut self, address: &str, value: Value) {
        self.last.insert(address.to_owned(), value);
    }

    /// Forget a value so the next equal value republishes.
    pub fn invalidate(&mut self, address: &str) {
        self.last.remove(address);
    }

    /// Forget all values under an address prefix (e.g. a cleared FX slot).
    pub fn clear_prefix(&mut self, prefix: &str) {
        self.last.retain(|addr, _| !addr.starts_with(prefix));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_slot_publishes_info() {
        let mut reg = FxRegistry::new();
        let updates = reg.observe_name(2, 1, "VST3: ReaEQ (Cockos)");
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            RegistryUpdate::PublishInfo { track, fx, payload } => {
                assert_eq!((*track, *fx), (2, 1));
                match payload {
                    Value::Map(m) => {
                        assert_eq!(m["name"], Value::String("VST3: ReaEQ (Cockos)".into()));
                        assert_eq!(m["param_count"], Value::Integer(0));
                    }
                    _ => panic!("Expected Map payload"),
                }
            }
            other => panic!("Unexpected update: {other:?}"),
        }
    }

    #[test]
    fn unchanged_name_is_a_no_op() {
        let mut reg = FxRegistry::new();
        assert_eq!(reg.observe_name(2, 1, "VST3: ReaEQ (Cockos)").len(), 1);
        assert_eq!(reg.observe_name(2, 1, "VST3: ReaEQ (Cockos)").len(), 0);
    }

    #[test]
    fn changed_name_republishes_and_clears_params() {
        let mut reg = FxRegistry::new();
        reg.observe_name(2, 1, "VST3: ReaEQ (Cockos)");
        reg.observe_param_name(2, 1, 0, "Freq 1");

        let updates = reg.observe_name(2, 1, "VST3: ReaComp (Cockos)");
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            RegistryUpdate::PublishInfo { payload, .. } => match payload {
                Value::Map(m) => {
                    assert_eq!(m["name"], Value::String("VST3: ReaComp (Cockos)".into()));
                    // Old plugin's parameter names are meaningless now.
                    assert_eq!(m["params"], Value::Map(BTreeMap::new()));
                }
                _ => panic!("Expected Map payload"),
            },
            other => panic!("Unexpected update: {other:?}"),
        }
        // The param name observed for the old plugin is gone.
        assert!(reg.entry(2, 1).unwrap().params.is_empty());
    }

    #[test]
    fn cleared_slot_emits_unsets() {
        let mut reg = FxRegistry::new();
        reg.observe_name(2, 1, "VST3: ReaEQ (Cockos)");
        reg.observe_param_name(2, 1, 0, "Freq 1");
        reg.observe_param_name(2, 1, 3, "Gain 1");

        let updates = reg.observe_name(2, 1, "");
        assert_eq!(updates.len(), 1);
        match &updates[0] {
            RegistryUpdate::ClearSlot {
                track,
                fx,
                unset_addresses,
            } => {
                assert_eq!((*track, *fx), (2, 1));
                // info + param values 0..=3 (max observed param index).
                assert_eq!(
                    unset_addresses,
                    &vec![
                        "/reaper/track/2/fx/1/info".to_string(),
                        "/reaper/track/2/fx/1/param/0/value".to_string(),
                        "/reaper/track/2/fx/1/param/1/value".to_string(),
                        "/reaper/track/2/fx/1/param/2/value".to_string(),
                        "/reaper/track/2/fx/1/param/3/value".to_string(),
                    ]
                );
            }
            other => panic!("Unexpected update: {other:?}"),
        }
        assert!(reg.entry(2, 1).is_none());
    }

    #[test]
    fn clearing_an_unknown_slot_is_a_no_op() {
        let mut reg = FxRegistry::new();
        assert_eq!(reg.observe_name(2, 1, "").len(), 0);
    }

    #[test]
    fn param_names_update_info() {
        let mut reg = FxRegistry::new();
        reg.observe_name(2, 1, "VST3: ReaEQ (Cockos)");

        assert_eq!(reg.observe_param_name(2, 1, 0, "Freq 1").len(), 1);
        // Unchanged param name is a no-op.
        assert_eq!(reg.observe_param_name(2, 1, 0, "Freq 1").len(), 0);

        let entry = reg.entry(2, 1).unwrap();
        assert_eq!(entry.params.get(&0).unwrap(), "Freq 1");
    }

    #[test]
    fn param_names_for_unknown_slot_are_ignored() {
        let mut reg = FxRegistry::new();
        assert_eq!(reg.observe_param_name(2, 1, 0, "Freq 1").len(), 0);
        assert!(reg.entry(2, 1).is_none());
    }

    #[test]
    fn value_cache_suppresses_duplicates() {
        let mut cache = ValueCache::new();
        assert!(cache.check_and_store("/a", Value::Integer(1)));
        assert!(!cache.check_and_store("/a", Value::Integer(1)));
        assert!(cache.check_and_store("/a", Value::Integer(2)));
    }

    #[test]
    fn value_cache_compares_floats_by_bits() {
        let mut cache = ValueCache::new();
        // REAPER echoes are f32-widened; identical bit patterns must dedupe.
        let v = Value::Float((0.75_f32 as f64).into());
        assert!(cache.check_and_store("/a", v.clone()));
        assert!(!cache.check_and_store("/a", v));
    }

    #[test]
    fn seeded_values_swallow_the_next_equal_feedback() {
        let mut cache = ValueCache::new();
        cache.seed("/a", Value::Float(0.5.into()));
        assert!(!cache.check_and_store("/a", Value::Float(0.5.into())));
        assert!(cache.check_and_store("/a", Value::Float(0.6.into())));
    }

    #[test]
    fn invalidation_and_prefix_clearing_allow_republish() {
        let mut cache = ValueCache::new();
        cache.check_and_store("/a", Value::Integer(1));
        cache.check_and_store("/fx/1/param/0/value", Value::Float(0.5.into()));
        cache.check_and_store("/fx/1/param/1/value", Value::Float(0.25.into()));

        cache.invalidate("/a");
        assert!(cache.check_and_store("/a", Value::Integer(1)));

        cache.clear_prefix("/fx/1/");
        assert!(cache.check_and_store("/fx/1/param/0/value", Value::Float(0.5.into())));
        assert!(cache.check_and_store("/fx/1/param/1/value", Value::Float(0.25.into())));
    }
}
