//! IPN 2-element legacy encoding filter
//!
//! This Egress WriteFilter rewrites IPN 3-element EIDs to legacy 2-element format
//! for peers that require the older encoding.

use super::*;
use hardy_bpv7::eid::Eid;

/// Configuration for IPN 2-element legacy encoding filter
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[derive(Default)]
pub struct Config {
    /// EID patterns for next-hops requiring legacy IPN encoding
    #[cfg_attr(feature = "serde", serde(rename = "legacy-nodes"))]
    legacy_nodes: Vec<hardy_eid_patterns::EidPattern>,
}

pub fn init(config: Config) -> filters::Filter {
    filters::Filter::Write(Arc::new(IpnLegacyFilter::new(config)))
}

/// Egress WriteFilter that rewrites IPN 3-element EIDs to legacy 2-element format
struct IpnLegacyFilter {
    peer_patterns: Vec<hardy_eid_patterns::EidPattern>,
}

impl IpnLegacyFilter {
    fn new(config: Config) -> Self {
        Self {
            peer_patterns: config.legacy_nodes,
        }
    }

    fn matches_next_hop(&self, next_hop: &Eid) -> bool {
        self.peer_patterns.iter().any(|p| p.matches(next_hop))
    }
}

#[async_trait]
impl WriteFilter for IpnLegacyFilter {
    async fn filter(
        &self,
        bundle: &bundle::Bundle,
        data: &[u8],
    ) -> Result<RewriteResult, bpa::Error> {
        // Check if next-hop requires legacy encoding
        let Some(next_hop) = &bundle.metadata.next_hop else {
            return Ok(RewriteResult::Continue(None, None));
        };

        if !self.matches_next_hop(next_hop) {
            return Ok(RewriteResult::Continue(None, None));
        }

        // Check if rewriting is needed
        let needs_source = matches!(bundle.bundle.id.source, Eid::Ipn { .. });
        let needs_dest = matches!(bundle.bundle.destination, Eid::Ipn { .. });

        if !needs_source && !needs_dest {
            return Ok(RewriteResult::Continue(None, None));
        }

        // Use Editor to rewrite EIDs
        let mut editor = hardy_bpv7::editor::Editor::new(&bundle.bundle, data);

        if let Eid::Ipn {
            fqnn,
            service_number,
        } = &bundle.bundle.id.source
        {
            editor = editor
                .with_source(Eid::LegacyIpn {
                    fqnn: fqnn.clone(),
                    service_number: *service_number,
                })
                .map_err(|(_, e)| e)?;
        }

        if let Eid::Ipn {
            fqnn,
            service_number,
        } = &bundle.bundle.destination
        {
            editor = editor
                .with_destination(Eid::LegacyIpn {
                    fqnn: fqnn.clone(),
                    service_number: *service_number,
                })
                .map_err(|(_, e)| e)?;
        }

        let new_data = editor.rebuild()?;

        Ok(RewriteResult::Continue(None, Some(new_data)))
    }
}
