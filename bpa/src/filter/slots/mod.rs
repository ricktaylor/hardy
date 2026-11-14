//! Annotation slots — embedder-private metadata carried in the bundle's
//! classification group.
//!
//! A custom filter pair (an ingress Classifier and an egress Rewriter shipped
//! together) declares a slot with [`slot!`](crate::slot): a `static`
//! [`Slot`] whose at-rest name is its declaring path, so no two
//! declarations can share a name — Rust's path namespace is the registry,
//! and nothing is registered or checked at run time. Access is Rust
//! visibility on the static: a pair shares state by naming the slot. The
//! scheme is cooperative, not cryptographic: trusted code could forge a
//! `Slot` for any name. The BPA carries the values opaquely as
//! canonically-encoded CBOR; the pair sees them fully typed.
//!
//! The BPA stores what a Classifier writes, persisted with every bundle and
//! held in memory with its record, and checks nothing: filters are trusted,
//! compiled-in code. A Classifier that derives a value from sender-chosen
//! data bounds it itself — by truncating, by hashing, or by storing a small
//! key and re-reading the bytes at the hook that consumes it.
//!
//! A slot value is a cache of a pure derivation over (stored bytes, chain,
//! config) — never a ledger. It is persisted with the bundle, in the clear:
//! the metadata store does not encrypt at rest, so a slot must never carry
//! BPSec-decrypted plaintext or other secrets. Clearing and
//! re-derivation at restart re-admission and policy-epoch bumps is settled
//! design, not yet wired (Phase 3, `filter_subsystem_design.md`): until
//! then a recovered bundle keeps its stored slot values. A value whose
//! declaration moved or disappeared is name-keyed at rest and unreadable,
//! and goes with the next clearing.

use core::{
    fmt::{self, Debug, Formatter},
    marker::PhantomData,
};

use hardy_bpv7::{bpsec::Roles, eid::Eid};
// `encode::Bytes` is aliased: in this crate a bare `Bytes` reads as the
// ubiquitous `bytes::Bytes` buffer, not a CBOR byte-string wrapper.
use hardy_cbor::{
    decode::{Error as DecodeError, FromCbor},
    encode::{Bytes as CborBytes, Encoder, ToCbor, emit},
};

use self::state::SlotWrite;
use crate::{HashMap, routing::TableId};

pub(crate) mod state;

/// Declares an annotation slot: a `static` [`Slot`] named, at rest, by its
/// declaring path — the module path and the static's identifier.
///
/// Declare slots at module scope. The name records the module, not the
/// function, so two same-named slots in different function bodies of one
/// module would share a name. Moving or renaming the declaration renames the
/// slot, so values stored under the old name become unreadable.
///
/// ```
/// use hardy_bpa::filter::slots::MetadataDelta;
///
/// hardy_bpa::slot!(static MARK: u32);
///
/// fn main() {
///     let mut delta = MetadataDelta::default();
///     delta.set(&MARK, &7);
/// }
/// ```
#[macro_export]
macro_rules! slot {
    ($(#[$meta:meta])* $vis:vis static $name:ident: $value:ty $(;)?) => {
        $(#[$meta])*
        $vis static $name: $crate::filter::slots::Slot<$value> =
            $crate::filter::slots::Slot::__declare(
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!($name)),
            );
    };
}

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
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// One annotation slot holding a `T`, declared with [`slot!`](crate::slot).
///
/// Every read and write of the slot names it; Rust visibility on the
/// declaring static is the access control.
pub struct Slot<T> {
    name: &'static str,
    _value: PhantomData<fn() -> T>,
}

// Manual impl: a derived Debug would needlessly bound `T: Debug`.
impl<T> Debug for Slot<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot").field("name", &self.name).finish()
    }
}

impl<T> Slot<T> {
    /// The constructor behind [`slot!`](crate::slot), which passes the
    /// declaring path as `name`; declare slots with the macro.
    #[doc(hidden)]
    #[must_use]
    pub const fn __declare(name: &'static str) -> Self {
        Self {
            name,
            _value: PhantomData,
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        self.name
    }
}

/// A Classifier's requested metadata changes, applied by the engine after the
/// invocation returns.
///
/// Carries annotation-slot writes and the bundle's routing inputs; the
/// `class` field arrives with the policy tranche.
#[derive(Debug, Default)]
pub struct MetadataDelta {
    pub(crate) slots: Vec<SlotWrite>,
    /// The table half of the bundle's `{table, key}` routing inputs. `Some`
    /// writes the persisted value — per-field last-writer-wins across the
    /// sequential chain — and `None` expresses no opinion, preserving it.
    /// Table selection is a per-bundle routing decision, not a class
    /// property; a class needing a specific table is a classifier that emits
    /// it (`docs/routing_table_redesign.md`, "Multiple tables").
    pub route_table: Option<TableId>,
    /// The key half: the EID the RIB walk looks up in place of the
    /// destination. Same write semantics as `route_table`. Producers own the
    /// skip-self discipline — never emit a key that resolves locally for a
    /// bundle that must forward (`docs/routing_table_redesign.md`, "Key
    /// selection").
    pub route_key: Option<Eid>,
    /// Whether egress writes a Bundle Age block, from the clock, on a bundle
    /// with a creation time that carries none, for downstream nodes without a
    /// clock (RFC 9171 §5.5). Same write semantics as `route_table`; unset
    /// adds none. Ingress sets it, ahead of the chain, on a bundle whose
    /// Bundle Age it discards, which `Some(false)` clears. A Bundle Age the
    /// bundle carries is updated regardless (§5.4).
    pub add_bundle_age: Option<bool>,
    /// Whether this node reassembles the fragment, holding it until every
    /// fragment of its bundle has arrived and the whole can be rebuilt (RFC
    /// 9171 §5.9), where the fragment would otherwise be forwarded as
    /// routed. Same write semantics as `route_table`; unset leaves the
    /// decision to the route: the bundle's destination reassembles it, as
    /// RFC 9171 §5.9 requires of it, and a relay forwards it. `Some(true)`
    /// reassembles here a fragment the route would forward, the whole then
    /// routed as any bundle; `Some(false)` declines a fragment addressed
    /// here, which is then dropped at the gate and reported, since a raw
    /// fragment is never delivered and cannot usefully forward. Every
    /// fragment of a bundle gets the same answer only where the decision is
    /// derived from what every fragment shares, the primary block's fields,
    /// and never from a fragment's own offset or the extension blocks that
    /// travel in fragment 0 alone: a Classifier that answers differently
    /// for two fragments of one bundle splits it, some forwarded and the
    /// rest held until they expire.
    pub reassemble: Option<bool>,
    /// This node's security roles on the blocks, by block number: on each
    /// block's BCB operation (confidentiality) and BIB operation
    /// (integrity), whether this node is the operation's verifier or its
    /// acceptor (RFC 9172 §1.4), as a matter of its policy.
    ///
    /// As the acceptor of an operation (RFC 9172 §5.1.1, §5.1.2) the gate
    /// decrypts a block accepted for confidentiality and verifies one
    /// accepted for integrity, and the bundle is stored without the
    /// accepted operations; the BIB over a block accepted for
    /// confidentiality alone stays, in plaintext, checked where this node
    /// holds its key. Accepting a block's BCB operation takes with it the
    /// BCB operations on an encrypted BIB that covers the block and on that
    /// BIB's other targets, all or none (§3.9), so each of those targets
    /// must be announced for confidentiality too; and a block encrypted
    /// here cannot be accepted for integrity alone, since its BIB is
    /// encrypted with it. A block this node cannot accept, for want of a
    /// key, for a failed operation, for a target the walk reaches that is
    /// not announced, or for a service announced that the block does not
    /// carry, which the announcement expected ("Missing security
    /// operation", the target discarded as §5.1.1 and §5.1.2 recommend of
    /// one the policy expected protected), its flags decide, as for a block
    /// it cannot process (RFC 9171 §5.6 Step 4): the bundle is deleted, or
    /// the block discarded with its operations; the primary block, which
    /// the bundle cannot lose, deletes it. A BIB on the primary block is
    /// verified and removed, the primary left as received (RFC 9171
    /// §4.3.1). An announced payload is
    /// accepted as it streams, as a node that is not the destination
    /// processes a BCB where its security policy directs it (§5.1.1).
    ///
    /// As the verifier of an operation, a role a node that is not its
    /// acceptor "MAY attempt" (RFC 9172 §5.1.2) and one announced here is
    /// expected to fill, integrity is verified by the BIB operation this
    /// arrival checked and found good, the header pass's for a resident
    /// block, the drain's for a payload still streaming, and
    /// confidentiality by authenticating the block's ciphertext, decrypting
    /// it under the key this node holds, the plaintext going nowhere. A
    /// verification that passes changes nothing: the block travels as
    /// received, its operations on to their acceptor. One this node could
    /// not make, for want of a key, for an operation in a security context
    /// it does not support, or, for integrity, because the block is
    /// encrypted here, so that its BIB is encrypted with it (RFC 9172 §3.9)
    /// and unreadable without accepting it, is a block it cannot process
    /// (RFC 9171 §5.6 Step 4), whose flags decide as above, with the report
    /// "Block unsupported" or "Unknown security operation"; ciphertext that
    /// does not authenticate is a failed operation, "Failed security
    /// operation" (RFC 9172 §5.1.1); and a block that carries no operation
    /// of the service announced is discarded, as RFC 9172 §5.1.1 and §5.1.2
    /// recommend, or deletes the bundle where its flags say so, with the
    /// report "Missing security operation".
    ///
    /// Every role announced on a fragment is passed over: a fragment's
    /// operations are accepted or verified after reassembly, when the whole
    /// bundle crosses this gate (RFC 9172 §5.2); so is anything at the
    /// Originate hook, since a source accepts nothing. Announcements
    /// accumulate across the Ingress chain, one
    /// Classifier unable to withdraw another's, the acceptor's role
    /// outranking the verifier's on one operation, and serve the arrival
    /// only: they are never persisted. None reaches the per-hop blocks,
    /// whose operations the header pass accepts before the chain runs (RFC
    /// 9171 §5.4), nor the destination's acceptance of every operation
    /// (RFC 9172 §1.4), which outranks any verification announced.
    pub roles: HashMap<u64, Roles>,
}

impl MetadataDelta {
    /// Stages a slot write, encoding the value for at-rest storage.
    ///
    /// Within one delta the last write to a slot wins, mirroring the
    /// per-slot last-writer-wins rule across the sequential Classifier
    /// chain.
    pub fn set<T: SlotValue>(&mut self, slot: &Slot<T>, value: &T) {
        self.slots.push(SlotWrite {
            name: slot.name,
            value: emit(value).0.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    mod first {
        crate::slot!(pub static MARK: u32);
    }

    mod second {
        crate::slot!(pub static MARK: u32);
    }

    #[test]
    fn same_named_slots_in_different_modules_are_distinct() {
        assert_eq!(
            first::MARK.name(),
            "hardy_bpa::filter::slots::tests::first::MARK"
        );
        assert_ne!(first::MARK.name(), second::MARK.name());
    }
}
