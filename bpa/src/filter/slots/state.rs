//! Crate-internal slot machinery: builder-side registration, the frozen
//! table, and the bundle's at-rest classification state.

use core::{
    fmt::{self, Debug, Formatter},
    marker::PhantomData,
    num::NonZeroUsize,
};

use super::{Error, Result, SlotHandle, SlotValue};
use crate::{Arc, BTreeMap};

/// One staged slot write: the registered name and size bound travel with the
/// encoded value so application needs no table lookup.
pub struct SlotWrite {
    pub name: Arc<str>,
    pub max_size: NonZeroUsize,
    pub value: Box<[u8]>,
}

// Slot values are embedder data and never print: lengths only.
impl Debug for SlotWrite {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotWrite")
            .field("name", &self.name)
            .field("max_size", &self.max_size)
            .field("value_len", &self.value.len())
            .finish()
    }
}

/// At-rest slot storage: registered stable name → canonically-encoded value.
///
/// Name-keyed at rest so a value whose registration disappeared across a
/// restart is simply unreadable — no handle can name it — until pruned or
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

    pub fn insert(&mut self, name: Arc<str>, value: Box<[u8]>) {
        self.0.insert(name, value);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Drops every value whose name is not registered in `table` — the
    /// load-time half of the "unknown name dropped harmlessly" contract.
    #[allow(dead_code)] // wired by the restart re-admission path (Phase 3, filter_subsystem_design.md)
    pub fn retain_registered(&mut self, table: &SlotTable) {
        self.0.retain(|name, _| table.0.contains_key(name));
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

/// Builder-side collection of slot registrations; frozen into a [`SlotTable`]
/// by `build()`.
#[derive(Debug, Default)]
pub struct SlotRegistry(Vec<(Arc<str>, NonZeroUsize)>);

impl SlotRegistry {
    pub fn register<T: SlotValue>(&mut self, name: &str, max_size: NonZeroUsize) -> SlotHandle<T> {
        let name: Arc<str> = name.into();
        self.0.push((name.clone(), max_size));
        SlotHandle {
            name,
            max_size,
            _value: PhantomData,
        }
    }

    /// Merges another registry's registrations into this one, preserving their order.
    pub fn merge(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// Freezes the registrations, rejecting duplicate names loudly.
    pub fn freeze(self) -> Result<SlotTable> {
        let mut table = BTreeMap::new();
        for (name, max_size) in self.0 {
            if table.insert(name.clone(), max_size).is_some() {
                return Err(Error::DuplicateName(name));
            }
        }
        Ok(SlotTable(table))
    }
}

/// The frozen slot table: registered stable name → per-slot size bound.
#[derive(Debug)]
pub struct SlotTable(BTreeMap<Arc<str>, NonZeroUsize>);

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn debug_shows_lengths_never_values() {
        let mut map = SlotMap::default();
        map.insert("vendor.x".into(), b"embedder data".as_slice().into());
        assert_eq!(format!("{map:?}"), r#"{"vendor.x": 13}"#);

        let write = SlotWrite {
            name: "vendor.x".into(),
            max_size: bound(16),
            value: b"embedder data".as_slice().into(),
        };
        assert_eq!(
            format!("{write:?}"),
            r#"SlotWrite { name: "vendor.x", max_size: 16, value_len: 13 }"#
        );
    }

    #[test]
    fn freeze_rejects_duplicate_names() {
        let mut registry = SlotRegistry::default();
        let _a: SlotHandle<u32> = registry.register("vendor.x", bound(16));
        let _b: SlotHandle<u64> = registry.register("vendor.x", bound(32));

        let Err(Error::DuplicateName(name)) = registry.freeze() else {
            panic!("duplicate registration must fail to freeze");
        };
        assert_eq!(&*name, "vendor.x");
    }

    #[test]
    fn freeze_accepts_distinct_names() {
        let mut registry = SlotRegistry::default();
        let _a: SlotHandle<u32> = registry.register("vendor.x", bound(16));
        let _b: SlotHandle<u32> = registry.register("vendor.y", bound(16));

        let table = registry.freeze().unwrap();
        assert_eq!(table.0.len(), 2);
    }

    #[test]
    fn retain_registered_drops_unknown_names() {
        let mut registry = SlotRegistry::default();
        let _a: SlotHandle<u32> = registry.register("vendor.known", bound(16));
        let table = registry.freeze().unwrap();

        let mut slots = SlotMap::default();
        slots.insert("vendor.known".into(), Box::from(&[1u8][..]));
        slots.insert("vendor.forgotten".into(), Box::from(&[2u8][..]));

        slots.retain_registered(&table);

        assert!(slots.get("vendor.known").is_some());
        assert!(slots.get("vendor.forgotten").is_none());
    }
}
