use super::*;

/// Register all filters with the BPA.
pub fn register(config: &config::Config, bpa: &hardy_bpa::bpa::Bpa) -> anyhow::Result<()> {
    // RFC9171 validity filter (with custom config if provided, otherwise default)
    bpa.register_filter(
        hardy_bpa::filters::Hook::Ingress,
        "rfc9171-validity",
        &[],
        hardy_bpa::filters::Filter::Read(Arc::new(
            hardy_bpa::filters::rfc9171::Rfc9171ValidityFilter::new(&config.rfc9171_validity),
        )),
    )?;
    info!(
        "Registered RFC9171 validity filter (primary-block-integrity={}, bundle-age-required={})",
        config.rfc9171_validity.primary_block_integrity,
        config.rfc9171_validity.bundle_age_required
    );

    // IPN legacy filter for handling legacy IPN EID formats
    #[cfg(feature = "ipn-legacy-filter")]
    if let Some(filter) = hardy_ipn_legacy_filter::IpnLegacyFilter::new(&config.ipn_legacy_nodes) {
        bpa.register_filter(
            hardy_bpa::filters::Hook::Egress,
            "ipn-legacy",
            &[],
            hardy_bpa::filters::Filter::Write(filter),
        )?;
        info!(
            "Registered IPN legacy filter for {} pattern(s)",
            config.ipn_legacy_nodes.0.len()
        );
    }

    #[cfg(not(feature = "ipn-legacy-filter"))]
    if !config.ipn_legacy_nodes.0.is_empty() {
        warn!("Ignoring ipn-legacy-nodes configuration option as it is disabled at compile time");
    }

    Ok(())
}
