//! Annotation slots — embedder-private metadata carried in the bundle's
//! classification group.
//!
//! A custom filter pair (an ingress Classifier and an egress Rewriter shipped
//! together) registers a slot at BPA construction — a stable name plus a
//! typed, size-bounded value — and receives a typed [`SlotHandle`]. The
//! handle is the capability: possession grants access, so a pair shares state
//! by sharing the handle in its common construction code, and no other code
//! can name the slot. The BPA carries the values opaquely as
//! canonically-encoded CBOR; the pair sees them fully typed.
//!
//! A slot value is a cache of a pure derivation over (stored bytes, chain,
//! config) — never a ledger. It is persisted with the bundle, cleared and
//! re-derived at restart re-admission and policy-epoch bumps, and a value
//! whose registration disappeared across a restart is dropped harmlessly:
//! name-keyed at rest, it is unreadable without a handle and the engine's
//! re-admission path prunes it.

use core::{marker::PhantomData, num::NonZeroUsize};

// `encode::Bytes` is aliased: in this crate a bare `Bytes` reads as the
// ubiquitous `bytes::Bytes` buffer, not a CBOR byte-string wrapper.
use hardy_cbor::{
    decode::{Error as DecodeError, FromCbor},
    encode::{Bytes as CborBytes, Encoder, ToCbor, emit},
};
use thiserror::Error;

use crate::{Arc, BTreeMap};

/// Errors from annotation-slot registration.
#[derive(Debug, Error)]
pub enum Error {
    /// Two registrations share a stable name.
    #[error("Annotation slot '{0}' is registered more than once")]
    DuplicateName(Arc<str>),
}

pub type Result<T> = core::result::Result<T, Error>;

/// A value type storable in an annotation slot.
///
/// Blanket-implemented for every type that round-trips through the canonical
/// CBOR codec; implement [`ToCbor`] and [`FromCbor`] rather than this trait.
pub trait SlotValue: ToCbor + FromCbor<Error: From<DecodeError>> {}

impl<T> SlotValue for T
where
    T: ToCbor + FromCbor,
    T::Error: From<DecodeError>,
{
}

/// An owned byte-string slot value.
///
/// Bare byte containers are deliberately not [`SlotValue`]s: hardy-cbor
/// encodes `[u8]` with *array* semantics through its blanket slice impl and
/// reserves byte-string encoding for the explicit `encode::Bytes` wrapper —
/// which borrows, so it cannot round-trip as a stored value. `Blob` is the
/// owned, two-way counterpart: it encodes as a CBOR byte string and decodes
/// through the codec's `Box<[u8]>` byte-string impl, making an opaque blob
/// a first-class slot value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob(pub Box<[u8]>);

impl ToCbor for Blob {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) {
        CborBytes(&self.0).to_cbor(encoder);
    }
}

impl FromCbor for Blob {
    type Error = DecodeError;

    fn from_cbor(data: &[u8]) -> core::result::Result<(Self, bool, usize), Self::Error> {
        Box::<[u8]>::from_cbor(data).map(|(value, shortest, len)| (Self(value), shortest, len))
    }
}

/// Typed capability for one registered annotation slot.
///
/// Obtainable solely from
/// [`BpaBuilder::annotation_slot`](crate::builder::BpaBuilder::annotation_slot):
/// possession is the access control, so per-slot privacy needs no permission
/// machinery.
#[derive(Debug)]
pub struct SlotHandle<T> {
    name: Arc<str>,
    max_size: NonZeroUsize,
    _value: PhantomData<fn() -> T>,
}

// Manual impl: a derived Clone would needlessly bound `T: Clone`.
impl<T> Clone for SlotHandle<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            max_size: self.max_size,
            _value: PhantomData,
        }
    }
}

impl<T> SlotHandle<T> {
    pub(crate) fn name(&self) -> &Arc<str> {
        &self.name
    }
}

/// A Classifier's requested metadata changes, applied by the engine after the
/// invocation returns.
///
/// Carries annotation-slot writes only for now: the `class` and `route_key`
/// fields arrive additively with the policy and routing tranches
/// (`#[non_exhaustive]` keeps that growth non-breaking).
#[non_exhaustive]
#[derive(Debug, Default)]
pub struct MetadataDelta {
    pub(crate) slots: Vec<SlotWrite>,
}

impl MetadataDelta {
    /// Stages a slot write, encoding the value for at-rest storage.
    ///
    /// Within one delta the last write to a slot wins, mirroring the
    /// per-slot last-writer-wins rule across the sequential Classifier chain.
    pub fn set<T: SlotValue>(&mut self, handle: &SlotHandle<T>, value: &T) {
        self.slots.push(SlotWrite {
            name: handle.name.clone(),
            max_size: handle.max_size,
            value: emit(value).0.into(),
        });
    }
}

// One staged slot write: the registered name and size bound travel with the
// encoded value so application needs no table lookup.
#[derive(Debug)]
pub(crate) struct SlotWrite {
    pub(crate) name: Arc<str>,
    pub(crate) max_size: NonZeroUsize,
    pub(crate) value: Box<[u8]>,
}

/// At-rest slot storage: registered stable name → canonically-encoded value.
///
/// Name-keyed at rest so a value whose registration disappeared across a
/// restart is simply unreadable — no handle can name it — until pruned or
/// cleared for re-derivation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct SlotMap(BTreeMap<Arc<str>, Box<[u8]>>);

impl SlotMap {
    // Doubles as the serde skip predicate: an empty map serializes to
    // nothing, keeping records byte-identical to the pre-slots shape.
    #[allow(dead_code)] // referenced from the serde(skip_serializing_if) attribute
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&[u8]> {
        self.0.get(name).map(AsRef::as_ref)
    }

    pub(crate) fn insert(&mut self, name: Arc<str>, value: Box<[u8]>) {
        self.0.insert(name, value);
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    /// Drops every value whose name is not registered in `table` — the
    /// load-time half of the "unknown name dropped harmlessly" contract.
    #[allow(dead_code)] // called by the metadata load path when the engine swap (C3) lands
    pub(crate) fn retain_registered(&mut self, table: &SlotTable) {
        self.0.retain(|name, _| table.0.contains_key(name));
    }
}

/// Monotonic stamp of the policy configuration a bundle was last classified
/// under, driving lazy re-classification at restart re-admission.
///
/// Engine bookkeeping: no accessor, invisible outside the crate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub(crate) struct PolicyEpoch(pub(crate) u64);

impl PolicyEpoch {
    // Serde skip predicate: the initial epoch serializes to nothing, keeping
    // records byte-identical to the pre-slots shape.
    #[allow(dead_code)] // referenced from the serde(skip_serializing_if) attribute
    pub(crate) fn is_initial(&self) -> bool {
        self.0 == 0
    }
}

/// Builder-side collection of slot registrations; frozen into a [`SlotTable`]
/// by `build()`.
#[derive(Debug, Default)]
pub(crate) struct SlotRegistry(Vec<(Arc<str>, NonZeroUsize)>);

impl SlotRegistry {
    pub(crate) fn register<T: SlotValue>(
        &mut self,
        name: &str,
        max_size: NonZeroUsize,
    ) -> SlotHandle<T> {
        let name: Arc<str> = name.into();
        self.0.push((name.clone(), max_size));
        SlotHandle {
            name,
            max_size,
            _value: PhantomData,
        }
    }

    /// Freezes the registrations, rejecting duplicate names loudly.
    pub(crate) fn freeze(self) -> Result<SlotTable> {
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
pub(crate) struct SlotTable(BTreeMap<Arc<str>, NonZeroUsize>);

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
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
