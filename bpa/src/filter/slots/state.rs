//! Crate-internal slot machinery: the staged write and the bundle's at-rest
//! classification state.

use core::fmt::{self, Debug, Formatter};

use crate::{Arc, BTreeMap};

/// One staged slot write: the slot's declared name and its encoded value.
pub struct SlotWrite {
    pub name: &'static str,
    pub value: Box<[u8]>,
}

// Slot values are embedder data and never print: lengths only.
impl Debug for SlotWrite {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotWrite")
            .field("name", &self.name)
            .field("value_len", &self.value.len())
            .finish()
    }
}

/// At-rest slot storage: declared name → canonically-encoded value.
///
/// Name-keyed at rest so a value whose declaration moved or disappeared
/// across a restart is simply unreadable — no `Slot` names it — until
/// cleared for re-derivation.
#[derive(Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SlotMap(BTreeMap<Arc<str>, Box<[u8]>>);

// Slot values are embedder data and never print: each name maps to its
// value's length.
impl Debug for SlotMap {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(name, value)| (name, value.len())))
            .finish()
    }
}

impl SlotMap {
    // Doubles as the serde skip predicate: an empty map serializes to
    // nothing, keeping records byte-identical to the pre-slots shape.
    #[allow(dead_code)] // referenced from the serde(skip_serializing_if) attribute
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.0.get(name).map(AsRef::as_ref)
    }

    pub fn insert(&mut self, name: &str, value: Box<[u8]>) {
        self.0.insert(Arc::from(name), value);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// Monotonic stamp of the policy configuration a bundle was last classified
/// under, driving lazy re-classification at restart re-admission.
///
/// Engine bookkeeping: no accessor, invisible outside the crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PolicyEpoch(pub u64);

impl PolicyEpoch {
    // Serde skip predicate: the initial epoch serializes to nothing, keeping
    // records byte-identical to the pre-slots shape.
    #[allow(dead_code)] // referenced from the serde(skip_serializing_if) attribute
    pub fn is_initial(&self) -> bool {
        self.0 == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_lengths_never_values() {
        let mut map = SlotMap::default();
        map.insert("vendor::X", b"embedder data".as_slice().into());
        assert_eq!(format!("{map:?}"), r#"{"vendor::X": 13}"#);

        let write = SlotWrite {
            name: "vendor::X",
            value: b"embedder data".as_slice().into(),
        };
        assert_eq!(
            format!("{write:?}"),
            r#"SlotWrite { name: "vendor::X", value_len: 13 }"#
        );
    }
}
