mod metadata;
mod status;

pub(crate) mod parse;

pub use metadata::{BundleMetadata, ExtensionFields, Origin, WritableMetadata};
pub use status::BundleStatus;

use hardy_bpv7::{
    bundle::{Bundle as Bpv7Bundle, Id},
    eid::Eid,
    primary_block::PrimaryBlock,
};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// A bundle together with its BPA-local processing metadata and status.
///
/// Pairs the on-the-wire BPv7 bundle with [`BundleMetadata`] (persisted facts:
/// ingress context, decoded extension fields, filter annotations) and the
/// bundle's current [`BundleStatus`].
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Bundle {
    /// The parsed BPv7 bundle (primary block + blocks map).
    pub bpv7: Bpv7Bundle,
    /// BPA-local metadata: ingress info, decoded extension fields, annotations.
    pub metadata: BundleMetadata,
    /// Current processing status within the BPA pipeline. Pipeline state, not
    /// a persisted fact: excluded from the serialized record — metadata
    /// backends persist it out-of-band in typed, queryable columns and set
    /// this field when they decode.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub status: BundleStatus,
}

impl Bundle {
    /// Wraps a freshly parsed or built wire bundle with its metadata; status
    /// starts [`New`](BundleStatus::New).
    pub fn new(bpv7: Bpv7Bundle, metadata: BundleMetadata) -> Self {
        Self {
            bpv7,
            metadata,
            status: BundleStatus::default(),
        }
    }

    /// The bundle's ID (from the primary block).
    pub fn id(&self) -> &Id {
        &self.bpv7.primary.id
    }

    /// The bundle's primary block.
    pub fn primary(&self) -> &PrimaryBlock {
        &self.bpv7.primary
    }

    pub fn creation_time(&self) -> OffsetDateTime {
        self.primary()
            .id
            .timestamp
            .as_datetime()
            .unwrap_or_else(|| {
                self.metadata
                    .received_at()
                    // No clock: creation = received time − Bundle Age. Saturate an
                    // out-of-range age (the field is `pub`, so not necessarily
                    // wire-decoded) like `expiry()` saturates `lifetime`.
                    .saturating_sub(
                        self.metadata
                            .extensions
                            .age
                            .unwrap_or_default()
                            .try_into()
                            .unwrap_or(Duration::MAX),
                    )
            })
    }

    pub fn expiry(&self) -> OffsetDateTime {
        self.creation_time()
            .saturating_add(self.primary().lifetime.try_into().unwrap_or(Duration::MAX))
    }

    #[inline]
    pub fn has_expired(&self) -> bool {
        self.expiry() <= OffsetDateTime::now_utc()
    }

    /// Returns the EID of the node that forwarded this bundle.
    ///
    /// Prefers the Previous Node extension block (in-band), falling back to
    /// the CLA peer node ID (out-of-band). Per RFC 9171 Section 4.4.1, both
    /// identify the immediate 1-hop forwarding node when present.
    pub fn previous_node(&self) -> Option<Eid> {
        self.metadata
            .extensions
            .previous_node
            .clone()
            .or_else(|| match self.metadata.origin() {
                Origin::Ingress {
                    peer_node: Some(node),
                    ..
                } => Some(node.clone().into()),
                _ => None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    fn make_bundle(
        timestamp: CreationTimestamp,
        age: Option<core::time::Duration>,
        lifetime: core::time::Duration,
    ) -> Bundle {
        let mut metadata = BundleMetadata::originated();
        metadata.extensions.age = age;
        Bundle::new(
            Bpv7Bundle {
                primary: PrimaryBlock {
                    id: Id {
                        source: "ipn:0.99.1".parse().unwrap(),
                        timestamp,
                        fragment_info: None,
                    },
                    flags: Default::default(),
                    crc_type: Default::default(),
                    destination: "ipn:0.1.99".parse().unwrap(),
                    report_to: Default::default(),
                    lifetime,
                },
                blocks: Default::default(),
            },
            metadata,
        )
    }

    // When creation timestamp is zero (unknown), creation_time() should
    // fall back to received_at minus bundle age.
    #[test]
    fn test_age_fallback() {
        let age = core::time::Duration::from_secs(60);
        let bundle = make_bundle(
            CreationTimestamp::default(),
            Some(age),
            Duration::ZERO
                .try_into()
                .unwrap_or(core::time::Duration::from_secs(3600)),
        );

        // With zero timestamp, creation_time = received_at - age
        let expected = bundle
            .metadata
            .received_at()
            .saturating_sub(age.try_into().unwrap());
        let actual = bundle.creation_time();

        // Allow 1ms tolerance for test timing
        let diff = (actual - expected).abs();
        assert!(
            diff < Duration::milliseconds(1),
            "Age fallback: expected {expected}, got {actual}, diff {diff}"
        );
    }

    // Expiry = creation_time + lifetime
    #[test]
    fn test_expiry_calculation() {
        let lifetime = core::time::Duration::from_secs(3600);
        let bundle = make_bundle(CreationTimestamp::now(), None, lifetime);

        let creation = bundle.creation_time();
        let expiry = bundle.expiry();
        let diff = expiry - creation;

        // Should be exactly the lifetime (within 1ms tolerance)
        let expected = Duration::seconds(3600);
        assert!(
            (diff - expected).abs() < Duration::milliseconds(1),
            "Expiry should be creation + lifetime, got diff={diff}"
        );
    }
}
