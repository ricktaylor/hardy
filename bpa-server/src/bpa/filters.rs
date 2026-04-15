use super::*;

// Register all filters with the BPA.
pub fn register(
    rfc9171_validity: &hardy_bpa::filters::rfc9171::Config,
    #[cfg(feature = "ipn-legacy-filter")] ipn_legacy_nodes: &hardy_ipn_legacy_filter::Config,
    bpa: &hardy_bpa::bpa::Bpa,
) -> anyhow::Result<()> {
    bpa.register_filter(
        hardy_bpa::filters::Hook::Ingress,
        "rfc9171-validity",
        &[],
        hardy_bpa::filters::Filter::Read(Arc::new(
            hardy_bpa::filters::rfc9171::Rfc9171ValidityFilter::new(rfc9171_validity),
        )),
    )?;
    info!(
        "Registered RFC9171 validity filter (primary-block-integrity={}, bundle-age-required={})",
        rfc9171_validity.primary_block_integrity, rfc9171_validity.bundle_age_required
    );

    #[cfg(feature = "ipn-legacy-filter")]
    if let Some(filter) = hardy_ipn_legacy_filter::IpnLegacyFilter::new(ipn_legacy_nodes) {
        bpa.register_filter(
            hardy_bpa::filters::Hook::Egress,
            "ipn-legacy",
            &[],
            hardy_bpa::filters::Filter::Write(filter),
        )?;
        info!(
            "Registered IPN legacy filter for {} pattern(s)",
            ipn_legacy_nodes.0.len()
        );
    }

    Ok(())
}
