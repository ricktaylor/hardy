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

use super::*;

/// Configuration for the RFC9171 validity filter.
///
/// Each field controls whether a specific validation check is enabled.
/// By default, all checks are enabled for strict RFC9171 compliance.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    /// Check that the primary block has integrity protection (CRC or BIB coverage).
    ///
    /// RFC9171 §4.3.1: "A CRC SHALL be present in the primary block unless
    /// the bundle includes a BPSec Block Integrity Block whose target is the
    /// primary block"
    ///
    /// Disable this for interoperability with implementations that don't add CRC.
    #[cfg_attr(feature = "serde", serde(rename = "primary-block-integrity"))]
    pub primary_block_integrity: bool,

    /// Check that bundles without a clock have a Bundle Age block.
    ///
    /// RFC9171 §4.4.2: "If the bundle's creation time is zero, then the bundle
    /// MUST contain exactly one (1) occurrence of this type of block [Bundle Age]"
    ///
    /// Disable this for compatibility with RFC9173 Appendix A test vectors.
    #[cfg_attr(feature = "serde", serde(rename = "bundle-age-required"))]
    pub bundle_age_required: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            primary_block_integrity: true,
            bundle_age_required: true,
        }
    }
}

/// RFC9171 validity filter that enforces bundle policy requirements.
///
/// This filter is auto-registered at the Ingress hook when the `rfc9171-filter`
/// feature is enabled (default). The auto-registered instance uses [`Config::default()`],
/// which enables all checks.
///
/// To customize the checks, create a filter with a specific configuration:
///
/// ```ignore
/// use hardy_bpa::filters::rfc9171::{Config, Rfc9171ValidityFilter};
///
/// let config = Config {
///     primary_block_integrity: true,
///     bundle_age_required: false, // Disable for RFC9173 test vectors
/// };
///
/// bpa.register_filter(
///     filters::Hook::Ingress,
///     "rfc9171-validity",
///     &[],
///     filters::Filter::Read(Arc::new(Rfc9171ValidityFilter::new(config))),
/// )?;
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Rfc9171ValidityFilter {
    config: Config,
}

impl Default for Rfc9171ValidityFilter {
    fn default() -> Self {
        Self::new(&Config::default())
    }
}

impl Rfc9171ValidityFilter {
    /// Creates a new RFC9171 validity filter with the given configuration.
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
        }
    }

    /// Returns a reference to the filter's configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }
}

#[async_trait]
impl ReadFilter for Rfc9171ValidityFilter {
    async fn filter(
        &self,
        bundle: &bundle::Bundle,
        _data: &[u8],
    ) -> Result<FilterResult, crate::Error> {
        // RFC9171 §4.3.1: Primary block integrity check
        if self.config.primary_block_integrity {
            if let Some(primary_block) = bundle.bundle.blocks.get(&0) {
                let has_crc = !matches!(bundle.bundle.crc_type, hardy_bpv7::crc::CrcType::None);
                let has_bib = !matches!(primary_block.bib, hardy_bpv7::block::BibCoverage::None);

                if !has_crc && !has_bib {
                    debug!(
                        bundle_id = %bundle.bundle.id,
                        "Rejecting bundle: primary block has no integrity protection (no CRC, no BIB)"
                    );
                    return Ok(FilterResult::Drop(Some(
                        hardy_bpv7::status_report::ReasonCode::BlockUnintelligible,
                    )));
                }
            }
        }

        // RFC9171 §4.4.2: Bundle Age required when no clock
        if self.config.bundle_age_required
            && !bundle.bundle.id.timestamp.is_clocked()
            && bundle.bundle.age.is_none()
        {
            debug!(
                bundle_id = %bundle.bundle.id,
                "Rejecting bundle: no clock in creation timestamp and no Bundle Age block"
            );
            return Ok(FilterResult::Drop(Some(
                hardy_bpv7::status_report::ReasonCode::LifetimeExpired,
            )));
        }

        Ok(FilterResult::Continue)
    }
}
