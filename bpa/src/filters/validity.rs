/*!
Bundle validity filter - enforces basic BPv7 validity checks.

This filter rejects bundles that have expired or exceeded their hop limit.
These are fundamental protocol checks that apply regardless of deployment policy.
*/

use hardy_async::async_trait;
use hardy_bpv7::status_report::ReasonCode;
use tracing::debug;

use super::{ReadFilter, ReadResult};
use crate::bundle::Bundle;

/// Bundle validity filter that enforces lifetime and hop-count checks.
///
/// Auto-registered at the Ingress hook before other filters.
#[derive(Debug, Clone, Default)]
pub struct BundleValidityFilter;

#[async_trait]
impl ReadFilter for BundleValidityFilter {
    async fn filter(&self, bundle: &Bundle, _data: &[u8]) -> Result<ReadResult, crate::Error> {
        if let Some(u) = bundle.bundle.flags.unrecognised {
            debug!(
                bundle_id = %bundle.bundle.id,
                "Bundle primary block has unrecognised flag bits set: {u:#x}"
            );
        }

        if bundle.has_expired() {
            debug!(
                bundle_id = %bundle.bundle.id,
                "Rejecting bundle: lifetime has expired"
            );
            return Ok(ReadResult::Drop(Some(ReasonCode::LifetimeExpired)));
        }

        if let Some(hop_info) = bundle.bundle.hop_count.as_ref() {
            if hop_info.count > hop_info.limit {
                debug!(
                    bundle_id = %bundle.bundle.id,
                    limit = hop_info.limit,
                    count = hop_info.count,
                    "Rejecting bundle: hop limit exceeded"
                );
                return Ok(ReadResult::Drop(Some(ReasonCode::HopLimitExceeded)));
            }
        }

        Ok(ReadResult::Continue)
    }
}
