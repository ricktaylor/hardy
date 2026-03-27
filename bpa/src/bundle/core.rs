use hardy_bpv7::bundle::Bundle as Bpv7Bundle;
use hardy_bpv7::eid::Eid;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use super::metadata::BundleMetadata;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Bundle {
    pub bundle: Bpv7Bundle,
    pub metadata: BundleMetadata,
}

impl Bundle {
    pub fn creation_time(&self) -> OffsetDateTime {
        self.bundle.id.timestamp.as_datetime().unwrap_or_else(|| {
            self.metadata
                .read_only
                .received_at
                // The following unwrap() is safe, as bundle.age is u64::MAX millisecs
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

    /// Returns the next-hop EID set by the RIB for this dispatch cycle.
    ///
    /// This is transient derived data — set by `rib::find()` and consumed by
    /// egress filters. Not persisted.
    pub fn next_hop(&self) -> Option<&Eid> {
        self.metadata.read_only.next_hop.as_ref()
    }

    /// Returns the EID of the node that forwarded this bundle.
    ///
    /// Prefers the Previous Node extension block (in-band), falling back to
    /// the CLA peer node ID (out-of-band). Per RFC 9171 Section 4.4.1, both
    /// identify the immediate 1-hop forwarding node when present.
    pub fn previous_node(&self) -> Option<Eid> {
        self.bundle.previous_node.clone().or_else(|| {
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
    // use super::*;

    // // TODO: Implement test for 'Age Fallback' (Verify creation time derived from Age)
    // #[test]
    // fn test_age_fallback() {
    //     todo!("Verify creation time derived from Age");
    // }

    // // TODO: Implement test for 'Expiry Calculation' (Verify expiry time summation)
    // #[test]
    // fn test_expiry_calculation() {
    //     todo!("Verify expiry time summation");
    // }
}
