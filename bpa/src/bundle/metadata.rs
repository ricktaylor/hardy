use hardy_bpv7::{
    eid::{Eid, NodeId},
    hop_info::HopInfo,
};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Arc, cla::ClaAddress};

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
// restart re-admission. Fields arrive with the filter and policy tranches
// (see bpa/docs/filter_subsystem_redesign.md).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
struct Classification {}

/// Mutable annotations that filters may modify during bundle processing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct WritableMetadata {
    /// Optional flow label for QoS differentiation.
    pub flow_label: Option<u32>,
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
    /// Next-hop EID resolved by the RIB for the dispatch in progress; consumed
    /// by the forwarding path, recomputed on re-dispatch. Never persisted.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub next_hop: Option<Eid>,
    /// Mutable annotations that filters may update during processing.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub writable: WritableMetadata,
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
            next_hop: None,
            writable: WritableMetadata::default(),
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
}
