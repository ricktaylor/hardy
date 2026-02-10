//! IPN 2-element legacy encoding filter
//!
//! This Egress WriteFilter rewrites IPN 3-element EIDs to legacy 2-element format
//! for peers that require the older encoding.
//!

use hardy_bpa::async_trait;
use std::sync::Arc;

/// Configuration for IPN 2-element legacy encoding filter
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[derive(Default)]
pub struct Config(
    /// EID patterns for next-hops requiring legacy IPN encoding
    Vec<hardy_eid_patterns::EidPattern>,
);

pub fn init(
    bpa: &Arc<hardy_bpa::bpa::Bpa>,
    config: Config,
) -> Result<(), hardy_bpa::filters::Error> {
    if config.0.is_empty() {
        // Ignore empty vec
        return Ok(());
    }

    let filter = Arc::new(IpnLegacyFilter {
        peer_patterns: config.0,
    });

    bpa.register_filter(
        hardy_bpa::filters::Hook::Egress,
        "ipn-legacy",
        &[],
        hardy_bpa::filters::Filter::Write(filter),
    )
}

/// Egress WriteFilter that rewrites IPN 3-element EIDs to legacy 2-element format
struct IpnLegacyFilter {
    peer_patterns: Vec<hardy_eid_patterns::EidPattern>,
}

#[async_trait]
impl hardy_bpa::filters::WriteFilter for IpnLegacyFilter {
    async fn filter(
        &self,
        bundle: &hardy_bpa::bundle::Bundle,
        data: &[u8],
    ) -> Result<hardy_bpa::filters::RewriteResult, hardy_bpa::Error> {
        // Check if next-hop requires legacy encoding
        let Some(next_hop) = &bundle.metadata.read_only.next_hop else {
            return Ok(hardy_bpa::filters::RewriteResult::Continue(None, None));
        };

        if !self.peer_patterns.iter().any(|p| p.matches(next_hop)) {
            return Ok(hardy_bpa::filters::RewriteResult::Continue(None, None));
        }

        // Check if rewriting is needed
        let needs_source = matches!(bundle.bundle.id.source, hardy_bpv7::eid::Eid::Ipn { .. });
        let needs_dest = matches!(bundle.bundle.destination, hardy_bpv7::eid::Eid::Ipn { .. });

        if !needs_source && !needs_dest {
            return Ok(hardy_bpa::filters::RewriteResult::Continue(None, None));
        }

        // Use Editor to rewrite EIDs
        let mut editor = hardy_bpv7::editor::Editor::new(&bundle.bundle, data);

        if let hardy_bpv7::eid::Eid::Ipn {
            fqnn,
            service_number,
        } = &bundle.bundle.id.source
        {
            editor = editor
                .with_source(hardy_bpv7::eid::Eid::LegacyIpn {
                    fqnn: fqnn.clone(),
                    service_number: *service_number,
                })
                .map_err(|(_, e)| e)?;
        }

        if let hardy_bpv7::eid::Eid::Ipn {
            fqnn,
            service_number,
        } = &bundle.bundle.destination
        {
            editor = editor
                .with_destination(hardy_bpv7::eid::Eid::LegacyIpn {
                    fqnn: fqnn.clone(),
                    service_number: *service_number,
                })
                .map_err(|(_, e)| e)?;
        }

        let new_data = editor.rebuild()?;

        Ok(hardy_bpa::filters::RewriteResult::Continue(
            None,
            Some(new_data),
        ))
    }
}
