mod metadata;
mod status;

pub(crate) mod parse;

pub use self::metadata::{BundleMetadata, ExtensionFields, Origin, WritableMetadata};
pub use self::status::BundleStatus;

use hardy_bpv7::{
    bundle::{Bundle as Bpv7Bundle, BundleId, PrimaryBlock},
    eid::Eid,
};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

/// A bundle together with its BPA-local processing metadata and status.
///
/// Pairs the on-the-wire BPv7 bundle with [`BundleMetadata`] (persisted facts:
/// ingress context, decoded extension fields, filter annotations) and the
/// bundle's current [`BundleStatus`].
///
/// `Bundle` itself is never (de)serialized: persistence deals in
/// [`StoredBundle`]/[`StoredBundleRef`], whose only exit demands the status
/// from the backend's typed columns.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The parsed BPv7 bundle (primary block + blocks map).
    pub bpv7: Bpv7Bundle,
    /// BPA-local metadata: ingress info, decoded extension fields, annotations.
    pub metadata: BundleMetadata,
    /// Current processing status within the BPA pipeline. Never persisted
    /// in the record blob: backends re-impose it from their typed status
    /// columns via [`StoredBundle::into_bundle`].
    pub status: BundleStatus,
}

/// The persisted half of a [`Bundle`] — everything except the processing
/// status, which backends encode in their own typed columns.
///
/// [`into_bundle`](Self::into_bundle) is the only way back to a [`Bundle`],
/// so re-imposing the status at every deserialize site is a compile-time
/// obligation in every current and future backend: a forgotten status is a
/// missing argument, not a silently defaulted `New`.
#[cfg(feature = "serde")]
#[derive(Deserialize)]
pub struct StoredBundle {
    bpv7: Bpv7Bundle,
    metadata: BundleMetadata,
}

#[cfg(feature = "serde")]
impl StoredBundle {
    /// Recompose the record, re-imposing the status the backend holds in
    /// its typed columns.
    pub fn into_bundle(self, status: BundleStatus) -> Bundle {
        Bundle {
            bpv7: self.bpv7,
            metadata: self.metadata,
            status,
        }
    }
}

/// The borrowing serializer for [`StoredBundle`]'s on-disk shape.
#[cfg(feature = "serde")]
#[derive(Serialize)]
pub struct StoredBundleRef<'a> {
    bpv7: &'a Bpv7Bundle,
    metadata: &'a BundleMetadata,
}

#[cfg(feature = "serde")]
impl<'a> From<&'a Bundle> for StoredBundleRef<'a> {
    fn from(bundle: &'a Bundle) -> Self {
        Self {
            bpv7: &bundle.bpv7,
            metadata: &bundle.metadata,
        }
    }
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
    pub fn id(&self) -> &BundleId {
        &self.bpv7.primary.id
    }

    /// The bundle's primary block.
    pub fn primary(&self) -> &PrimaryBlock {
        &self.bpv7.primary
    }

    /// The bundle's creation time.
    ///
    /// For an unclocked source (a zero creation timestamp), falls back to
    /// [`received_at`](BundleMetadata::received_at) minus the Bundle Age
    /// extension field ([`ExtensionFields::age`]) — the RFC 9171 recovery of
    /// creation time on a node with no clock.
    pub fn creation_time(&self) -> OffsetDateTime {
        creation_time(
            self.primary(),
            self.metadata.extensions.age,
            self.metadata.received_at(),
        )
    }

    /// When the bundle's lifetime ends: [`creation_time`](Self::creation_time)
    /// plus the primary block's lifetime, saturating.
    pub fn expiry(&self) -> OffsetDateTime {
        expiry(
            self.primary(),
            self.metadata.extensions.age,
            self.metadata.received_at(),
        )
    }

    /// Whether [`expiry`](Self::expiry) has already passed.
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

/// The RFC 9171 creation-time rule, shared by [`Bundle::creation_time`] and
/// the pre-store ingress gate ([`parse::HeaderVerify::gate_reason`]): the
/// primary block's timestamp when the source is clocked, else `received_at`
/// minus the Bundle Age extension field. Saturates an out-of-range age (the
/// callers' age fields are `pub`, so not necessarily wire-decoded), like
/// [`expiry`] saturates `lifetime`.
///
/// `core::time::Duration` stays qualified in this module: the imported
/// `Duration` is `time::Duration`, the arithmetic type of the results.
pub(crate) fn creation_time(
    primary: &PrimaryBlock,
    age: Option<core::time::Duration>,
    received_at: OffsetDateTime,
) -> OffsetDateTime {
    primary.id.timestamp.as_datetime().unwrap_or_else(|| {
        received_at.saturating_sub(age.unwrap_or_default().try_into().unwrap_or(Duration::MAX))
    })
}

/// Expiry under the same rule: [`creation_time`] plus the primary block's
/// lifetime, saturating.
pub(crate) fn expiry(
    primary: &PrimaryBlock,
    age: Option<core::time::Duration>,
    received_at: OffsetDateTime,
) -> OffsetDateTime {
    creation_time(primary, age, received_at)
        .saturating_add(primary.lifetime.try_into().unwrap_or(Duration::MAX))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use hardy_bpv7::CreationTimestamp;

    // `core::time::Duration` stays qualified here too: `super::*` brings in
    // the `time::Duration` the expiry arithmetic uses.

    // Shared minimal bundle record: `now` timestamp, one-hour lifetime, no
    // extension blocks, `Originated` provenance, `New` status. Tests override
    // individual fields on the returned value.
    pub fn test_bundle(source: &str, destination: &str) -> Bundle {
        test_bundle_with_id(
            BundleId {
                source: source.parse().unwrap(),
                timestamp: CreationTimestamp::now(),
                fragment_info: None,
            },
            destination,
        )
    }

    // `test_bundle` with a caller-supplied full `BundleId` (fragment tests).
    pub fn test_bundle_with_id(id: BundleId, destination: &str) -> Bundle {
        Bundle::new(
            Bpv7Bundle {
                primary: PrimaryBlock {
                    id,
                    flags: Default::default(),
                    crc_type: Default::default(),
                    destination: destination.parse().unwrap(),
                    report_to: Default::default(),
                    lifetime: core::time::Duration::from_secs(3600),
                },
                blocks: Default::default(),
            },
            BundleMetadata::originated(),
        )
    }

    // `test_bundle` already past its expiry: zero lifetime and a
    // `received_at` in the past.
    pub fn test_expired_bundle(source: &str, destination: &str) -> Bundle {
        let mut bundle = test_bundle(source, destination);
        bundle.bpv7.primary.lifetime = core::time::Duration::ZERO;
        bundle.metadata = BundleMetadata::new(
            OffsetDateTime::now_utc() - Duration::seconds(10),
            Origin::Originated,
        );
        bundle
    }

    fn make_bundle(
        timestamp: CreationTimestamp,
        age: Option<core::time::Duration>,
        lifetime: core::time::Duration,
    ) -> Bundle {
        let mut bundle = test_bundle("ipn:0.99.1", "ipn:0.1.99");
        bundle.bpv7.primary.id.timestamp = timestamp;
        bundle.bpv7.primary.lifetime = lifetime;
        bundle.metadata.extensions.age = age;
        bundle
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
