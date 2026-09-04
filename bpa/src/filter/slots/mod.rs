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
use hardy_bpv7::eid::Eid;
use hardy_cbor::{
    decode::{Error as DecodeError, FromCbor},
    encode::{Bytes as CborBytes, Encoder, ToCbor, emit},
};
use thiserror::Error;

use self::state::SlotWrite;
use crate::Arc;

pub(crate) mod state;

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
/// [`FilterPack::annotation_slot`](crate::filter::pack::FilterPack::annotation_slot):
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
/// Carries annotation-slot writes and the bundle's routing inputs; the
/// `class` field arrives additively with the policy tranche
/// (`#[non_exhaustive]` keeps that growth non-breaking).
#[non_exhaustive]
#[derive(Debug, Default)]
pub struct MetadataDelta {
    pub(crate) slots: Vec<SlotWrite>,
    /// The table half of the bundle's `{table, key}` routing inputs. `Some`
    /// writes the persisted value — per-field last-writer-wins across the
    /// sequential chain — and `None` expresses no opinion, preserving it.
    /// Table selection is a per-bundle routing decision, not a class
    /// property; a class needing a specific table is a classifier that emits
    /// it (`docs/routing_table_redesign.md`, "Multiple tables").
    pub route_table: Option<u32>,
    /// The key half: the EID the RIB walk looks up in place of the
    /// destination. Same write semantics as `route_table`. Producers own the
    /// skip-self discipline — never emit a key that resolves locally for a
    /// bundle that must forward (`docs/routing_table_redesign.md`, "Key
    /// selection").
    pub route_key: Option<Eid>,
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
