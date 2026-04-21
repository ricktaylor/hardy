use hardy_bpv7::block::Payload;
use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::{Eid, NodeId};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::Arc;
use crate::cla::ClaAddress;

mod idle;
mod stored;

pub use idle::Idle;
pub use stored::Stored;

/// Processing status of a bundle within the BPA pipeline.
///
/// Tracks where a bundle is in the dispatch/forward/deliver lifecycle.
/// Persisted to metadata storage so processing can resume after restart.
#[derive(Default, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum BundleStatus {
    #[default]
    New,
    Dispatching,
    ForwardPending {
        peer: u32,
        queue: Option<u32>,
    },
    AduFragment {
        source: Eid,
        timestamp: CreationTimestamp,
    },
    Waiting,
    WaitingForService {
        service: Eid,
    },
}

/// Immutable ingress context captured when a bundle is first received.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ReadOnlyMetadata {
    pub received_at: OffsetDateTime,
    pub ingress_peer_node: Option<NodeId>,
    pub ingress_peer_addr: Option<ClaAddress>,
    #[cfg_attr(feature = "serde", serde(skip))]
    pub ingress_cla: Option<Arc<str>>,
    #[cfg_attr(feature = "serde", serde(skip))]
    pub next_hop: Option<Eid>,
}

impl Default for ReadOnlyMetadata {
    fn default() -> Self {
        Self {
            received_at: OffsetDateTime::now_utc(),
            ingress_peer_node: None,
            ingress_peer_addr: None,
            ingress_cla: None,
            next_hop: None,
        }
    }
}

/// Mutable annotations that filters may modify during bundle processing.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct WritableMetadata {
    pub flow_label: Option<u32>,
}

/// Combined metadata for a bundle held in the BPA.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct BundleMetadata {
    #[cfg_attr(feature = "serde", serde(skip))]
    pub status: BundleStatus,
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub read_only: ReadOnlyMetadata,
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub writable: WritableMetadata,
}

/// A bundle together with its BPA-local processing metadata.
///
/// Uses a typestate parameter to enforce the storage lifecycle at compile time:
///
/// - [`Bundle<Idle>`] — parsed and validated, carries raw data, not yet stored.
/// - [`Bundle<Stored>`] — data persisted, has `storage_name`. Can enter the pipeline.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Bundle<S = Idle> {
    /// The parsed BPv7 bundle (primary block, extension blocks, payload).
    pub bundle: Bpv7Bundle,
    /// BPA-local metadata: processing status, ingress context, annotations.
    pub metadata: BundleMetadata,
    /// Typestate-specific data.
    pub state: S,
}

impl<S> Bundle<S> {
    /// Returns the bundle's unique identifier.
    pub fn id(&self) -> &hardy_bpv7::bundle::Id {
        &self.bundle.id
    }

    pub fn creation_time(&self) -> OffsetDateTime {
        self.bundle.id.timestamp.as_datetime().unwrap_or_else(|| {
            self.metadata
                .read_only
                .received_at
                .saturating_sub(self.bundle.age.unwrap_or_default().try_into().unwrap())
        })
    }

    pub fn expiry(&self) -> OffsetDateTime {
        self.creation_time()
            .saturating_add(self.bundle.lifetime.try_into().unwrap_or(Duration::MAX))
    }

    #[inline]
    pub fn has_expired(&self) -> bool {
        self.expiry() <= OffsetDateTime::now_utc()
    }

    /// Extract the payload block data, decrypting if necessary.
    pub fn payload<'a>(
        &self,
        data: &'a [u8],
        key_source: &dyn KeySource,
    ) -> Result<Payload<'a>, hardy_bpv7::Error> {
        self.bundle.block_data(1, data, key_source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Bytes;
    use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    fn make_bundle(
        timestamp: CreationTimestamp,
        age: Option<core::time::Duration>,
        lifetime: core::time::Duration,
    ) -> Bundle<Idle> {
        Bundle::new(
            Bpv7Bundle {
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
                previous_node: None,
                age,
                hop_count: None,
                blocks: Default::default(),
            },
            Bytes::new(),
            None,
            None,
            None,
        )
    }

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

        let expected = bundle
            .metadata
            .read_only
            .received_at
            .saturating_sub(age.try_into().unwrap());
        let actual = bundle.creation_time();

        let diff = (actual - expected).abs();
        assert!(
            diff < Duration::milliseconds(1),
            "Age fallback: expected {expected}, got {actual}, diff {diff}"
        );
    }

    #[test]
    fn test_expiry_calculation() {
        let lifetime = core::time::Duration::from_secs(3600);
        let bundle = make_bundle(CreationTimestamp::now(), None, lifetime);

        let creation = bundle.creation_time();
        let expiry = bundle.expiry();
        let diff = expiry - creation;

        let expected = Duration::seconds(3600);
        assert!(
            (diff - expected).abs() < Duration::milliseconds(1),
            "Expiry should be creation + lifetime, got diff={diff}"
        );
    }
}
