use core::{str::from_utf8, time::Duration};

use hardy_bpv7::{
    eid::{Eid, NodeId},
    hop_info::HopInfo,
    primary_block::PrimaryBlock,
};
use hardy_cbor::decode::{Head, Marker, parse, parse_exact};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    Arc,
    cla::Address,
    filter::slots::{
        Blob, MetadataDelta, Slot, SlotValue,
        state::{PolicyEpoch, SlotMap},
    },
    routing::{RoutingKey, TableId},
};

/// A bundle's point of entry into this BPA's custody.
///
/// Part of the bundle's provenance: persisted and write-once. A bundle is
/// transit traffic if and only if its origin is
/// [`Ingress`](Origin::Ingress).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Origin {
    /// Received from a peer through a convergence-layer adapter.
    Ingress {
        /// Node ID of the peer that forwarded this bundle, from the CLA
        /// handshake.
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
        peer_addr: Option<Address>,
        /// Name of the CLA instance that received the bundle, a fact about
        /// the arrival that holds even once that instance is gone.
        cla: Arc<str>,
    },
    /// Sourced at this node: a service hand-in or a BPA-generated bundle.
    Originated,
    /// Recovered from bundle storage without a metadata record; the arrival
    /// facts are unrecoverable.
    Recovered,
}

// Arrival facts, written once at construction. Private fields and no `&mut`
// accessor keep the write-once property a compile-time one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
struct Provenance {
    received_at: OffsetDateTime,
    origin: Origin,
}

/// Decoded well-known extension-block fields, derived from the bundle's
/// bytes.
///
/// A cache rather than a source of truth: `bundle::parse` produces it and
/// records it here whenever the bundle's bytes are parsed, at ingress, on a
/// local build, or on a re-parse. It mirrors the *stored* bytes, which stay
/// as received, so the per-attempt rewrites at Egress and Deliver, the
/// per-hop update and a Rewriter's edits, must leave it alone: the expiry
/// arithmetic reads the as-received Bundle Age. Nothing re-validates it
/// afterwards.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ExtensionFields {
    /// EID of the node that last forwarded the bundle (Previous Node block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub previous_node: Option<Eid>,
    /// Age of the bundle, used when the source node has no clock (Bundle
    /// Age block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub age: Option<Duration>,
    /// Hop limit and current hop count for the bundle (Hop Count block).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub hop_count: Option<HopInfo>,
}

// Output of the Classifier chain: persisted, then cleared and re-derived at
// restart re-admission.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
struct Classification {
    // Annotation slots, embedder-private and gated by the declaring static.
    // The skip predicate keeps slot-free records byte-identical to the
    // pre-slots serde shape.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "SlotMap::is_empty")
    )]
    slots: SlotMap,
    // The {table, key} the chain derived, persisted as the cache of the
    // chain's output and read by the dispatcher at every lookup, the same
    // struct `Rib::find` consumes. The skip predicate keeps unkeyed records
    // byte-identical to the prior serde shape.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "RoutingKey::is_unset")
    )]
    routing: RoutingKey,
    // Policy-epoch stamp for lazy restart re-admission, engine bookkeeping
    // with no accessor.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "PolicyEpoch::is_initial")
    )]
    epoch: PolicyEpoch,
}

/// A bundle's BPA-local processing metadata.
///
/// The fields are partitioned by write discipline: the write-once arrival
/// facts, read through [`received_at`](Self::received_at) and
/// [`origin`](Self::origin); the [`extensions`](Self::extensions) cache of
/// parser-decoded extension fields; the classification group of annotation
/// slots and routing inputs, written only through the crate-private
/// delta-apply path; and the crate-private storage references. There is no
/// `Default`, since a defaulted provenance would fabricate a `received_at`
/// and an origin, so records are built through the constructors.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct BundleMetadata {
    // Write-once arrival facts.
    #[cfg_attr(feature = "serde", serde(flatten))]
    provenance: Provenance,
    /// Parser-derived cache of decoded extension-block fields.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub extensions: ExtensionFields,
    // Classifier-chain output: annotation slots and the routing inputs.
    #[cfg_attr(feature = "serde", serde(flatten))]
    classification: Classification,
    // Block removals derived at ingress: the failure-drops of RFC 9172
    // Section 5.1.1 and the honoured `delete_block_on_failure` unknowns.
    // The stored bundle is kept as received, so these ride the record and
    // are applied per forwarding or delivery attempt, at the egress rewrite
    // and the deliver strip. They are a cache of a pure derivation over the
    // received bytes and the keys, re-derived at restart re-admission, and
    // written by fixed machinery rather than by a filter.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Vec::is_empty")
    )]
    pub(crate) to_remove: Vec<u64>,
    // Opaque key the storage backend locates the bundle's bytes by.
    pub(crate) storage_name: Option<Arc<str>>,
}

impl BundleMetadata {
    /// Creates a record from explicit arrival facts.
    ///
    /// For reconstructing a record, in a storage backend or a test fixture,
    /// and for the BPA's own recovery paths. Live traffic uses
    /// [`ingress`](Self::ingress) or [`originated`](Self::originated), which
    /// stamp the current time.
    pub fn new(received_at: OffsetDateTime, origin: Origin) -> Self {
        Self {
            provenance: Provenance {
                received_at,
                origin,
            },
            extensions: ExtensionFields::default(),
            classification: Classification::default(),
            to_remove: Vec::new(),
            storage_name: None,
        }
    }

    /// Creates the record for a bundle arriving from a peer, stamping the
    /// current time.
    pub fn ingress(cla: Arc<str>, peer_node: Option<NodeId>, peer_addr: Option<Address>) -> Self {
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

    /// Sets the parse-derived extension fields, keeping construction a
    /// single expression.
    #[must_use]
    pub fn with_extensions(mut self, extensions: ExtensionFields) -> Self {
        self.extensions = extensions;
        self
    }

    /// Wall-clock time when the bundle entered this BPA's custody.
    pub fn received_at(&self) -> OffsetDateTime {
        self.provenance.received_at
    }

    /// The bundle's creation time under the RFC 9171 rule.
    ///
    /// The primary block's timestamp when the source is clocked, otherwise
    /// [`received_at`](Self::received_at) less the Bundle Age extension
    /// field. It lives here because both fallback inputs are this record's,
    /// the primary block being the caller's argument.
    pub fn creation_time(&self, primary: &PrimaryBlock) -> OffsetDateTime {
        super::creation_time(primary, self.extensions.age, self.received_at())
    }

    /// The end of the bundle's lifetime,
    /// [`creation_time`](Self::creation_time) plus the primary block's
    /// lifetime, saturating.
    pub fn expiry(&self, primary: &PrimaryBlock) -> OffsetDateTime {
        super::expiry(primary, self.extensions.age, self.received_at())
    }

    /// Whether [`expiry`](Self::expiry) has already passed.
    pub fn has_expired(&self, primary: &PrimaryBlock) -> bool {
        self.expiry(primary) <= OffsetDateTime::now_utc()
    }

    /// The bundle's point of entry into this BPA's custody.
    pub fn origin(&self) -> &Origin {
        &self.provenance.origin
    }

    /// The `{table, key}` lookup inputs the Classifier chain derived, which
    /// the dispatcher hands to the RIB at every lookup.
    pub(crate) fn routing_key(&self) -> &RoutingKey {
        &self.classification.routing
    }

    /// The Classifier-derived routing key, if one is set.
    ///
    /// The RIB walks this EID in place of the destination.
    pub fn route_key(&self) -> Option<&Eid> {
        self.classification.routing.key.as_ref()
    }

    /// The Classifier-derived routing table, if one is set.
    pub fn route_table(&self) -> Option<&TableId> {
        self.classification.routing.table.as_ref()
    }

    /// Reads an annotation slot, decoding the stored value.
    ///
    /// Only code that can name the [`Slot`] reads it. Returns `None` when
    /// the slot is unset, and also when the stored bytes no longer decode as
    /// `T`: a stale value, left by a declaration whose type changed across a
    /// restart, reads as unset rather than erroring, and the next
    /// classification pass re-derives it.
    pub fn slot<T: SlotValue>(&self, slot: &Slot<T>) -> Option<T> {
        parse_exact(self.classification.slots.get(slot.name())?).ok()
    }

    /// Reads a text slot as a borrowed view of the stored bytes, without a
    /// copy.
    ///
    /// [`set`](MetadataDelta::set) encodes canonically, so a stored text
    /// value is a definite-length string whose payload sits contiguous in
    /// the record, and the returned `&str` borrows it directly. Returns
    /// `None` when the slot is unset, or when the stored bytes are not
    /// exactly one untagged definite-length text string.
    pub fn slot_str(&self, slot: &Slot<String>) -> Option<&str> {
        let data = self.classification.slots.get(slot.name())?;
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

    /// Reads a blob slot as a borrowed view of the stored bytes; see
    /// [`slot_str`](Self::slot_str).
    pub fn slot_bytes(&self, slot: &Slot<Blob>) -> Option<&[u8]> {
        let data = self.classification.slots.get(slot.name())?;
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

    /// Applies a Classifier's delta, the only write path into the
    /// classification group.
    ///
    /// The last writer wins, per slot and per routing field: a `Some`
    /// routing field writes, `None` expresses no opinion and preserves the
    /// stored value. Every slot write is stored as written, filters being
    /// trusted code that bounds what it writes.
    pub(crate) fn apply(&mut self, delta: MetadataDelta) {
        for write in delta.slots {
            self.classification.slots.insert(write.name, write.value);
        }
        if let Some(table) = delta.route_table {
            self.classification.routing.table = Some(table);
        }
        if let Some(key) = delta.route_key {
            self.classification.routing.key = Some(key);
        }
    }

    /// Clears the classification group for re-derivation, at restart
    /// re-admission or a policy-epoch bump, stamping the epoch the next
    /// classification pass runs under.
    #[allow(dead_code)] // wired by the restart re-admission path (Phase 3, filter_subsystem_design.md)
    pub(crate) fn clear_classification(&mut self, epoch: PolicyEpoch) {
        self.classification.slots.clear();
        self.classification.routing = RoutingKey::default();
        self.classification.epoch = epoch;
    }
}

#[cfg(test)]
mod tests {
    use crate::filter::slots::Blob;

    use super::*;

    // Slots built by name: the stale-type tests need two slots sharing a
    // name, which `slot!` cannot declare.
    fn handle<T: SlotValue>(name: &'static str) -> Slot<T> {
        Slot::__declare(name)
    }

    // Populated metadata survives the serde path both SQL backends persist
    // records through, and a row carrying `"flow_label":null`, written
    // before `WritableMetadata` was removed, still loads.
    #[cfg(feature = "serde")]
    #[test]
    fn populated_metadata_round_trips_through_serde() {
        let mut metadata = BundleMetadata::originated();
        let mut delta = MetadataDelta::default();
        delta.set(&handle::<u32>("vendor.mark"), &7);
        delta.route_table = Some(TableId::new(3));
        delta.route_key = Some("ipn:9.0".parse().unwrap());
        metadata.apply(delta);
        metadata.to_remove = vec![4, 5];

        let json = serde_json::to_string(&metadata).unwrap();
        let loaded: BundleMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, metadata);

        let mut legacy: serde_json::Value = serde_json::from_str(&json).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .insert("flow_label".into(), serde_json::Value::Null);
        let loaded: BundleMetadata = serde_json::from_value(legacy).unwrap();
        assert_eq!(loaded, metadata);
    }

    #[test]
    fn slot_round_trips_through_delta_apply() {
        let h = handle::<u32>("vendor.x");
        let mut md = BundleMetadata::originated();

        let mut delta = MetadataDelta::default();
        delta.set(&h, &42u32);
        md.apply(delta);

        assert_eq!(md.slot(&h), Some(42));
    }

    #[test]
    fn owned_container_slot_values_round_trip() {
        // A String rides the codec's owned decode through the blanket
        // SlotValue, and an opaque byte blob rides the Blob newtype; bare
        // byte containers are deliberately not SlotValues, see Blob's docs.
        let h_text = handle::<String>("vendor.text");
        let h_blob = handle::<Blob>("vendor.blob");
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

        // The borrowed views read straight out of the record.
        assert_eq!(md.slot_str(&h_text), Some("parsed-at-ingress"));
        assert_eq!(
            md.slot_bytes(&h_blob),
            Some(&[0xDEu8, 0xAD, 0xBE, 0xEF][..])
        );
    }

    #[test]
    fn borrowed_slot_views_read_stale_types_as_unset() {
        // A u32 written under the name: neither borrowed view matches.
        let h_writer = handle::<u32>("vendor.x");
        let h_text = handle::<String>("vendor.x");
        let h_blob = handle::<Blob>("vendor.x");
        let mut md = BundleMetadata::originated();

        let mut delta = MetadataDelta::default();
        delta.set(&h_writer, &42u32);
        md.apply(delta);

        assert_eq!(md.slot_str(&h_text), None);
        assert_eq!(md.slot_bytes(&h_blob), None);
    }

    #[test]
    fn later_write_wins_within_and_across_deltas() {
        let h = handle::<u32>("vendor.x");
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
    fn routing_delta_applies_per_field() {
        let key_a: Eid = "ipn:0.2.1".parse().unwrap();
        let key_b: Eid = "ipn:0.3.1".parse().unwrap();
        let mut md = BundleMetadata::originated();
        assert!(md.route_key().is_none());
        assert!(md.route_table().is_none());

        // Key alone: the table half is untouched.
        md.apply(MetadataDelta {
            route_key: Some(key_a.clone()),
            ..Default::default()
        });
        assert_eq!(md.route_key(), Some(&key_a));
        assert_eq!(md.route_table(), None);

        // Table alone: `None` on the key is no opinion, so the stored key
        // survives.
        md.apply(MetadataDelta {
            route_table: Some(TableId::new(7)),
            ..Default::default()
        });
        assert_eq!(md.route_table(), Some(&TableId::new(7)));
        assert_eq!(md.route_key(), Some(&key_a));

        // Last writer wins, per field.
        md.apply(MetadataDelta {
            route_key: Some(key_b.clone()),
            ..Default::default()
        });
        assert_eq!(md.route_key(), Some(&key_b));
        assert_eq!(md.route_table(), Some(&TableId::new(7)));
    }

    #[test]
    fn clear_classification_resets_the_routing_inputs() {
        let mut md = BundleMetadata::originated();

        md.apply(MetadataDelta {
            route_table: Some(TableId::new(1)),
            route_key: Some("ipn:0.2.1".parse().unwrap()),
            ..Default::default()
        });

        md.clear_classification(PolicyEpoch(2));

        assert!(md.route_key().is_none());
        assert!(md.route_table().is_none());
    }

    #[test]
    fn clear_classification_wipes_slots_and_stamps_the_epoch() {
        let h = handle::<u32>("vendor.x");
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
        let h_writer = handle::<u32>("vendor.x");
        let h_reader = handle::<bool>("vendor.x");
        let mut md = BundleMetadata::originated();

        // A declaration whose type changed across a restart: the stored
        // bytes decode as the old type, not the new one.
        let mut delta = MetadataDelta::default();
        delta.set(&h_writer, &42u32);
        md.apply(delta);

        assert_eq!(md.slot(&h_reader), None);
    }
}
