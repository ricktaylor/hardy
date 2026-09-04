use core::str::from_utf8;

use hardy_bpv7::{
    eid::{Eid, NodeId},
    hop_info::HopInfo,
};
use hardy_cbor::decode::{Head, Marker, parse, parse_exact};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::warn;

use crate::{
    Arc,
    cla::ClaAddress,
    filter::slots::{
        Blob, MetadataDelta, SlotHandle, SlotValue,
        state::{PolicyEpoch, SlotMap},
    },
};

/// How a bundle entered this BPA's custody.
///
/// Part of the bundle's provenance: persisted, write-once. At Egress the
/// transit predicate is a type-level match: a bundle is transit traffic iff
/// its origin is [`Ingress`](Origin::Ingress).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Origin {
    /// Received from a peer through a convergence-layer adapter.
    Ingress {
        /// Node ID of the peer that forwarded this bundle (from the CLA handshake).
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        peer_node: Option<NodeId>,
        /// Convergence-layer address of the ingress peer.
        #[cfg_attr(
            feature = "serde",
            serde(default, skip_serializing_if = "Option::is_none")
        )]
        peer_addr: Option<ClaAddress>,
        /// Name of the CLA instance that received the bundle — a fact about
        /// arrival, valid even if that CLA instance no longer exists.
        cla: Arc<str>,
    },
    /// Sourced at this node: a service hand-in or a BPA-generated bundle.
    Originated,
    /// Recovered from bundle storage without a metadata record; the arrival
    /// facts are unrecoverable.
    Recovered,
}

// Arrival facts, written once at construction. Private fields and no `&mut`
// accessor anywhere keep the write-once property a compile-time fact.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
struct Provenance {
    received_at: OffsetDateTime,
    origin: Origin,
}

/// Decoded well-known extension-block fields, derived from the bundle's bytes.
///
/// Produced by the parse pipelines (`bundle::parse`) and recorded here when
/// the bundle's content is parsed — at ingress, on local build, or on
/// re-parse. Never invalidated: the stored bytes are immutable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ExtensionFields {
    /// EID of the node that last forwarded the bundle (Previous Node block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub previous_node: Option<Eid>,
    /// Age of the bundle, used when the source node has no clock (Bundle Age block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub age: Option<core::time::Duration>,
    /// Hop limit and current hop count for the bundle (Hop Count block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub hop_count: Option<HopInfo>,
}

// Output of the classifier chain: persisted, cleared and re-derived at
// restart re-admission. The class and route_key fields arrive with the
// policy and routing tranches (see bpa/docs/filter_subsystem_design.md).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
struct Classification {
    // Registered annotation slots — embedder-private, gated per-handle. The
    // skip predicate keeps slot-free records byte-identical to the
    // pre-slots serde shape.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "SlotMap::is_empty")
    )]
    slots: SlotMap,
    // Policy-epoch stamp for lazy restart re-admission — engine bookkeeping
    // with no accessor at all.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "PolicyEpoch::is_initial")
    )]
    epoch: PolicyEpoch,
}

/// A bundle's BPA-local processing metadata.
///
/// Partitioned by write discipline: provenance (write-once arrival facts,
/// read via [`received_at`](Self::received_at) / [`origin`](Self::origin)),
/// the [`extensions`](Self::extensions) cache of parser-decoded extension
/// fields, the classification group (placeholder), and BPA infrastructure
/// references (crate-private). There is no `Default`: a defaulted provenance
/// would fabricate a `received_at` and an origin, so records are built only
/// through the constructors.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct BundleMetadata {
    // Write-once arrival facts.
    #[cfg_attr(feature = "serde", serde(flatten))]
    provenance: Provenance,
    /// Parser-derived cache of decoded extension-block fields.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub extensions: ExtensionFields,
    // Classifier-chain output; empty until the filter tranches land.
    #[cfg_attr(feature = "serde", serde(flatten))]
    classification: Classification,
    // Opaque key used by the storage backend to locate the serialised bundle data.
    pub(crate) storage_name: Option<Arc<str>>,
}

impl BundleMetadata {
    /// Creates a record from explicit arrival facts.
    ///
    /// For record reconstruction (storage backends, test fixtures) and the
    /// BPA's own recovery paths. Live traffic uses [`ingress`](Self::ingress)
    /// or [`originated`](Self::originated), which stamp the current time.
    pub fn new(received_at: OffsetDateTime, origin: Origin) -> Self {
        Self {
            provenance: Provenance {
                received_at,
                origin,
            },
            extensions: ExtensionFields::default(),
            classification: Classification::default(),
            storage_name: None,
        }
    }

    /// Creates the record for a bundle arriving from a peer, stamping the
    /// current time.
    pub fn ingress(
        cla: Arc<str>,
        peer_node: Option<NodeId>,
        peer_addr: Option<ClaAddress>,
    ) -> Self {
        Self::new(
            OffsetDateTime::now_utc(),
            Origin::Ingress {
                peer_node,
                peer_addr,
                cla,
            },
        )
    }

    /// Creates the record for a bundle sourced at this node, stamping the
    /// current time.
    pub fn originated() -> Self {
        Self::new(OffsetDateTime::now_utc(), Origin::Originated)
    }

    /// Wall-clock time when the bundle entered this BPA's custody.
    pub fn received_at(&self) -> OffsetDateTime {
        self.provenance.received_at
    }

    /// How the bundle entered this BPA's custody.
    pub fn origin(&self) -> &Origin {
        &self.provenance.origin
    }

    /// Reads an annotation slot, decoding the stored value.
    ///
    /// Handle-gated: only holders of the slot's [`SlotHandle`] can read it.
    /// Returns `None` when the slot is unset — or when the stored bytes no
    /// longer decode as `T` (a stale value from a registration that changed
    /// across a restart reads as unset rather than erroring; the next
    /// classification pass re-derives it).
    pub fn slot<T: SlotValue>(&self, handle: &SlotHandle<T>) -> Option<T> {
        parse_exact(self.classification.slots.get(handle.name())?).ok()
    }

    /// Reads a text slot as a borrowed view of the stored bytes — the
    /// zero-copy read.
    ///
    /// [`set`](MetadataDelta::set) encodes canonically, so a stored text
    /// value is a definite-length string whose payload sits contiguous in
    /// the record; the returned `&str` borrows it directly. `None` when the
    /// slot is unset or the stored bytes are not exactly one untagged
    /// definite-length text string.
    pub fn slot_str(&self, handle: &SlotHandle<String>) -> Option<&str> {
        let data = self.classification.slots.get(handle.name())?;
        let (head, _, offset) = parse::<(Head, bool, usize)>(data).ok()?;
        if !head.tags.is_empty() {
            return None;
        }
        let Marker::Text(Some(len)) = head.marker else {
            return None;
        };
        let end = offset.checked_add(usize::try_from(len).ok()?)?;
        if end != data.len() {
            return None;
        }
        from_utf8(&data[offset..end]).ok()
    }

    /// Reads a blob slot as a borrowed view of the stored bytes — the
    /// zero-copy read; see [`slot_str`](Self::slot_str).
    pub fn slot_bytes(&self, handle: &SlotHandle<Blob>) -> Option<&[u8]> {
        let data = self.classification.slots.get(handle.name())?;
        let (head, _, offset) = parse::<(Head, bool, usize)>(data).ok()?;
        if !head.tags.is_empty() {
            return None;
        }
        let Marker::Bytes(Some(len)) = head.marker else {
            return None;
        };
        let end = offset.checked_add(usize::try_from(len).ok()?)?;
        if end != data.len() {
            return None;
        }
        data.get(offset..end)
    }

    /// Applies a Classifier's delta — the only write path into the
    /// classification group.
    ///
    /// Per-slot last-writer-wins. A value exceeding its slot's registered
    /// size bound is dropped with a warning — never stored — keeping
    /// metadata stores honest; an already-stored value survives the dropped
    /// write.
    pub(crate) fn apply(&mut self, delta: MetadataDelta) {
        for write in delta.slots {
            if write.value.len() > write.max_size.get() {
                warn!(
                    "Annotation slot '{}' write of {} bytes exceeds its registered bound of {}; dropped",
                    write.name,
                    write.value.len(),
                    write.max_size
                );
            } else {
                self.classification.slots.insert(write.name, write.value);
            }
        }
    }

    /// Clears the classification group for re-derivation (restart
    /// re-admission or a policy-epoch bump), stamping the epoch the next
    /// classification pass runs under.
    #[allow(dead_code)] // wired to the re-admission path by the engine swap (C3)
    pub(crate) fn clear_classification(&mut self, epoch: PolicyEpoch) {
        self.classification.slots.clear();
        self.classification.epoch = epoch;
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;

    use crate::filter::slots::{Blob, state::SlotRegistry};

    use super::*;

    fn handle<T: SlotValue>(name: &str, max_size: usize) -> SlotHandle<T> {
        SlotRegistry::default().register(name, NonZeroUsize::new(max_size).unwrap())
    }

    #[test]
    fn slot_round_trips_through_delta_apply() {
        let h = handle::<u32>("vendor.x", 16);
        let mut md = BundleMetadata::originated();

        let mut delta = MetadataDelta::default();
        delta.set(&h, &42u32);
        md.apply(delta);

        assert_eq!(md.slot(&h), Some(42));
    }

    #[test]
    fn owned_container_slot_values_round_trip() {
        // The vendor-facing cases: a String rides the codec's owned
        // decode directly through the blanket SlotValue, and an opaque
        // byte blob rides the slots-provided Blob newtype (bare byte
        // containers deliberately are not SlotValues — see Blob's docs).
        let h_text = handle::<String>("vendor.text", 64);
        let h_blob = handle::<Blob>("vendor.blob", 64);
        let mut md = BundleMetadata::originated();

        let mut delta = MetadataDelta::default();
        delta.set(&h_text, &"parsed-at-ingress".to_string());
        delta.set(&h_blob, &Blob(Box::from(&[0xDEu8, 0xAD, 0xBE, 0xEF][..])));
        md.apply(delta);

        assert_eq!(md.slot(&h_text), Some("parsed-at-ingress".to_string()));
        assert_eq!(
            md.slot(&h_blob),
            Some(Blob(Box::from(&[0xDEu8, 0xAD, 0xBE, 0xEF][..])))
        );

        // The zero-copy reads: borrowed views straight into the record.
        assert_eq!(md.slot_str(&h_text), Some("parsed-at-ingress"));
        assert_eq!(
            md.slot_bytes(&h_blob),
            Some(&[0xDEu8, 0xAD, 0xBE, 0xEF][..])
        );
    }

    #[test]
    fn borrowed_slot_views_read_stale_types_as_unset() {
        // A u32 written under the name: neither borrowed view matches.
        let h_writer = handle::<u32>("vendor.x", 16);
        let h_text = handle::<String>("vendor.x", 16);
        let h_blob = handle::<Blob>("vendor.x", 16);
        let mut md = BundleMetadata::originated();

        let mut delta = MetadataDelta::default();
        delta.set(&h_writer, &42u32);
        md.apply(delta);

        assert_eq!(md.slot_str(&h_text), None);
        assert_eq!(md.slot_bytes(&h_blob), None);
    }

    #[test]
    fn later_write_wins_within_and_across_deltas() {
        let h = handle::<u32>("vendor.x", 16);
        let mut md = BundleMetadata::originated();

        // Within one delta: the second set wins.
        let mut delta = MetadataDelta::default();
        delta.set(&h, &1u32);
        delta.set(&h, &2u32);
        md.apply(delta);
        assert_eq!(md.slot(&h), Some(2));

        // Across sequential deltas (the chain rule): the later delta wins.
        let mut delta = MetadataDelta::default();
        delta.set(&h, &3u32);
        md.apply(delta);
        assert_eq!(md.slot(&h), Some(3));
    }

    #[test]
    fn oversized_write_is_dropped_and_preserves_the_stored_value() {
        // Bound of 4 bytes: a small integer encodes in one byte and fits;
        // u64::MAX encodes in nine and must not.
        let h = handle::<u64>("vendor.x", 4);
        let mut md = BundleMetadata::originated();

        let mut delta = MetadataDelta::default();
        delta.set(&h, &7u64);
        md.apply(delta);
        assert_eq!(md.slot(&h), Some(7));

        let mut delta = MetadataDelta::default();
        delta.set(&h, &u64::MAX);
        md.apply(delta);

        // The oversized write is dropped, not truncated, and does not
        // clobber the previously stored value.
        assert_eq!(md.slot(&h), Some(7));
    }

    #[test]
    fn clear_classification_wipes_slots_and_stamps_the_epoch() {
        let h = handle::<u32>("vendor.x", 16);
        let mut md = BundleMetadata::originated();

        let mut delta = MetadataDelta::default();
        delta.set(&h, &42u32);
        md.apply(delta);

        md.clear_classification(PolicyEpoch(3));

        assert_eq!(md.slot(&h), None);
        assert_eq!(md.classification.epoch, PolicyEpoch(3));
    }

    #[test]
    fn stale_bytes_of_another_type_read_as_unset() {
        let h_writer = handle::<u32>("vendor.x", 16);
        let h_reader = handle::<bool>("vendor.x", 16);
        let mut md = BundleMetadata::originated();

        // A registration that changed type across a restart: the stored
        // bytes decode as the old type, not the new one.
        let mut delta = MetadataDelta::default();
        delta.set(&h_writer, &42u32);
        md.apply(delta);

        assert_eq!(md.slot(&h_reader), None);
    }
}
