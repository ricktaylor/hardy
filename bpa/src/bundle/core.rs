use hardy_bpv7::eid::Eid;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use super::metadata::BundleMetadata;

/// A bundle together with its BPA-local processing metadata.
///
/// Pairs the on-the-wire BPv7 bundle with [`BundleMetadata`] that tracks
/// ingress context, processing status, and filter annotations.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Bundle {
    /// The parsed BPv7 bundle (primary block + blocks map).
    pub bundle: hardy_bpv7::bundle::Bundle,
    /// BPA-local metadata: ingress info, decoded extension fields, status, annotations.
    pub metadata: BundleMetadata,
}

impl Bundle {
    pub fn creation_time(&self) -> OffsetDateTime {
        self.bundle
            .primary
            .id
            .timestamp
            .as_datetime()
            .unwrap_or_else(|| {
                self.metadata
                    .read_only
                    .received_at
                    // No clock: creation = received time − Bundle Age.
                    .saturating_sub(
                        self.metadata
                            .read_only
                            .age
                            .unwrap_or_default()
                            .try_into()
                            .expect("bundle age in ms is within time::Duration's range"),
                    )
            })
    }

    pub fn expiry(&self) -> OffsetDateTime {
        self.creation_time().saturating_add(
            self.bundle
                .primary
                .lifetime
                .try_into()
                .unwrap_or(Duration::MAX),
        )
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
        self.metadata.read_only.previous_node.clone().or_else(|| {
            self.metadata
                .read_only
                .ingress_peer_node
                .clone()
                .map(Into::into)
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
        let mut metadata = BundleMetadata::default();
        metadata.read_only.age = age;
        Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                primary: hardy_bpv7::primary_block::PrimaryBlock {
                    id: hardy_bpv7::bundle::Id {
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
        }
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
            .read_only
            .received_at
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
