/*!
IPN 2-element legacy encoding filter

This Egress WriteFilter rewrites IPN 3-element EIDs to legacy 2-element format
for peers that require the older encoding.
*/

use hardy_bpa::{
    async_trait,
    bundle::Bundle,
    filter::{WriteFilter, WriteResult},
};
use hardy_bpv7::{
    editor::{Chunk, Editor},
    eid::Eid,
};

/// Configuration for IPN 2-element legacy encoding filter
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
#[derive(Default)]
pub struct Config(
    /// EID patterns for next-hops requiring legacy IPN encoding
    pub Vec<hardy_eid_patterns::EidPattern>,
);

/// Egress WriteFilter that rewrites IPN 3-element EIDs to legacy 2-element format.
///
/// # Example
///
/// ```ignore
/// let filter = IpnLegacyFilter::new(peer_patterns);
/// bpa.register_filter(
///     hardy_bpa::filter::Hook::Egress,
///     "ipn-legacy",
///     &[],
///     hardy_bpa::filter::Filter::Write(Arc::new(filter)),
/// )?;
/// ```
pub struct IpnLegacyFilter {
    peer_patterns: Vec<hardy_eid_patterns::EidPattern>,
}

impl IpnLegacyFilter {
    /// Create a new IPN legacy encoding filter.
    ///
    /// The caller should check that `peer_patterns` is not empty before
    /// constructing the filter (an empty filter would be a no-op).
    pub fn new(peer_patterns: Vec<hardy_eid_patterns::EidPattern>) -> Self {
        Self { peer_patterns }
    }
}

#[async_trait]
impl WriteFilter for IpnLegacyFilter {
    async fn filter(&self, bundle: &Bundle, data: &[u8]) -> Result<WriteResult, hardy_bpa::Error> {
        let Some(next_hop) = &bundle.metadata.next_hop else {
            return Ok(WriteResult::Continue(None, None));
        };

        if !self.peer_patterns.iter().any(|p| p.matches(next_hop)) {
            return Ok(WriteResult::Continue(None, None));
        }

        let needs_source = matches!(bundle.id().source, Eid::Ipn { .. });
        let needs_dest = matches!(bundle.primary().destination, Eid::Ipn { .. });

        if !needs_source && !needs_dest {
            return Ok(WriteResult::Continue(None, None));
        }

        // Editor needs a `&Bundle`, so re-parse structurally.
        let hardy_bpv7::parse::Parsed {
            data, bundle: raw, ..
        } = hardy_bpv7::parse::parse(hardy_bpa::Bytes::copy_from_slice(data))
            .map_err(hardy_bpv7::editor::Error::from)?;
        let mut editor = Editor::new(&raw, &data);

        if let Eid::Ipn {
            fqnn,
            service_number,
        } = &bundle.id().source
        {
            editor = editor
                .with_source(Eid::LegacyIpn {
                    fqnn: *fqnn,
                    service_number: *service_number,
                })
                .map_err(|(_, e)| e)?;
        }

        if let Eid::Ipn {
            fqnn,
            service_number,
        } = &bundle.primary().destination
        {
            editor = editor
                .with_destination(Eid::LegacyIpn {
                    fqnn: *fqnn,
                    service_number: *service_number,
                })
                .map_err(|(_, e)| e)?;
        }

        let data = editor.rebuild().map(|c| Chunk::flatten(c, &data))?;

        Ok(WriteResult::Continue(None, Some(data.into())))
    }
}
