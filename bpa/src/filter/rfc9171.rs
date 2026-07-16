/*!
RFC9171 validity filter - enforces bundle validity requirements from RFC9171.

This filter checks policy requirements that go beyond structural validity:
- Primary block integrity protection (CRC or BIB coverage)
- Bundle Age block presence when source has no clock

These checks are separated from the parser because:
1. They are policy decisions that deployments may need to disable
2. They can fail valid test vectors (e.g., RFC9173 Appendix A examples)
3. Different deployments may have different interoperability requirements
*/

use hardy_async::async_trait;
use hardy_bpv7::block::BibCoverage;
use hardy_bpv7::crc::CrcType;
use hardy_bpv7::status_report::ReasonCode;
use tracing::debug;

use super::{ReadFilter, ReadResult};
use crate::bundle::Bundle;

// All checks are enabled by default, for strict RFC9171 compliance.
const DEFAULT_PRIMARY_BLOCK_INTEGRITY: bool = true;
const DEFAULT_BUNDLE_AGE_REQUIRED: bool = true;

/// RFC9171 validity filter that enforces bundle policy requirements.
///
/// This filter is auto-registered at the Ingress hook unless the
/// `no-rfc9171-autoregister` feature is enabled; the auto-registered
/// instance enables all checks.
///
/// To customize the checks, chain the setter for each check to override:
///
/// ```ignore
/// use hardy_bpa::filter::rfc9171::Rfc9171ValidityFilter;
///
/// // Disable the Bundle Age check for RFC9173 test vectors.
/// let filter = Rfc9171ValidityFilter::new().bundle_age_required(false);
///
/// bpa.register_filter(
///     filter::Hook::Ingress,
///     "rfc9171-validity",
///     &[],
///     filter::Filter::Read(Arc::new(filter)),
/// )?;
/// ```
#[derive(Debug, Clone)]
pub struct Rfc9171ValidityFilter {
    // Check that the primary block has integrity protection (CRC or BIB
    // coverage). RFC9171 §4.3.1: "A CRC SHALL be present in the primary
    // block unless the bundle includes a BPSec Block Integrity Block whose
    // target is the primary block". Disabled for interoperability with
    // implementations that don't add a CRC.
    primary_block_integrity: bool,

    // Check that bundles without a clock have a Bundle Age block.
    // RFC9171 §4.4.2: "If the bundle's creation time is zero, then the
    // bundle MUST contain exactly one (1) occurrence of this type of block
    // [Bundle Age]". Disabled for compatibility with RFC9173 Appendix A
    // test vectors.
    bundle_age_required: bool,
}

impl Default for Rfc9171ValidityFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl Rfc9171ValidityFilter {
    /// Creates a new RFC9171 validity filter with every check enabled.
    pub fn new() -> Self {
        Self {
            primary_block_integrity: DEFAULT_PRIMARY_BLOCK_INTEGRITY,
            bundle_age_required: DEFAULT_BUNDLE_AGE_REQUIRED,
        }
    }

    /// Sets whether the primary block must carry integrity protection.
    pub fn primary_block_integrity(mut self, enabled: bool) -> Self {
        self.primary_block_integrity = enabled;
        self
    }

    /// Sets whether clockless bundles must carry a Bundle Age block.
    pub fn bundle_age_required(mut self, enabled: bool) -> Self {
        self.bundle_age_required = enabled;
        self
    }
}

#[async_trait]
impl ReadFilter for Rfc9171ValidityFilter {
    async fn filter(&self, bundle: &Bundle, _data: &[u8]) -> Result<ReadResult, crate::Error> {
        // RFC9171 §4.3.1: Primary block integrity check
        if self.primary_block_integrity
            && let Some(primary_block) = bundle.bundle.blocks.get(&0)
        {
            let has_crc = !matches!(bundle.bundle.primary.crc_type, CrcType::None);
            let has_bib = !matches!(primary_block.bib, BibCoverage::None);

            if !has_crc && !has_bib {
                debug!(
                    bundle_id = %bundle.bundle.primary.id,
                    "Rejecting bundle: primary block has no integrity protection (no CRC, no BIB)"
                );
                return Ok(ReadResult::Drop(Some(ReasonCode::BlockUnintelligible)));
            }
        }

        // RFC9171 §4.4.2: Bundle Age required when no clock
        if self.bundle_age_required
            && !bundle.bundle.primary.id.timestamp.is_clocked()
            && bundle.metadata.wire.age.is_none()
        {
            debug!(
                bundle_id = %bundle.bundle.primary.id,
                "Rejecting bundle: no clock in creation timestamp and no Bundle Age block"
            );
            return Ok(ReadResult::Drop(Some(ReasonCode::LifetimeExpired)));
        }

        Ok(ReadResult::Continue)
    }
}
