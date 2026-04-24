/*!
IPN 2-element legacy encoding filter

This Egress WriteFilter rewrites IPN 3-element EIDs to legacy 2-element format
for peers that require the older encoding.
*/

use hardy_bpa::async_trait;
use hardy_bpa::bundle::Bundle;
use hardy_bpa::filter::{WriteFilter, WriteResult};
use hardy_bpv7::editor::Editor;
use hardy_bpv7::eid::Eid;

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
        let Some(next_hop) = &bundle.metadata.read_only.next_hop else {
            return Ok(WriteResult::Continue(None, None));
        };

        if !self.peer_patterns.iter().any(|p| p.matches(next_hop)) {
            return Ok(WriteResult::Continue(None, None));
        }

        let needs_source = matches!(bundle.bundle.id.source, Eid::Ipn { .. });
        let needs_dest = matches!(bundle.bundle.destination, Eid::Ipn { .. });

        if !needs_source && !needs_dest {
            return Ok(WriteResult::Continue(None, None));
        }

        let mut editor = Editor::new(&bundle.bundle, data);

        if let Eid::Ipn {
            fqnn,
            service_number,
        } = &bundle.bundle.id.source
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
        } = &bundle.bundle.destination
        {
            editor = editor
                .with_destination(Eid::LegacyIpn {
                    fqnn: *fqnn,
                    service_number: *service_number,
                })
                .map_err(|(_, e)| e)?;
        }

        let new_data = editor.rebuild()?;

        Ok(WriteResult::Continue(None, Some(new_data.into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardy_bpa::bundle::BundleMetadata;
    use hardy_bpv7::builder::Builder;
    use hardy_bpv7::bundle::ParsedBundle;
    use hardy_bpv7::creation_timestamp::CreationTimestamp;

    fn make_config(patterns: &[&str]) -> Config {
        Config(patterns.iter().map(|p| p.parse().unwrap()).collect())
    }

    fn make_bundle(source: &str, dest: &str, next_hop: Option<&str>) -> (Bundle, Vec<u8>) {
        let src: Eid = source.parse().unwrap();
        let dst: Eid = dest.parse().unwrap();

        let (bpv7_bundle, data) = Builder::new(src, dst)
            .with_payload(std::borrow::Cow::Borrowed(b"test"))
            .build(CreationTimestamp::now())
            .unwrap();

        let mut metadata = BundleMetadata::default();
        metadata.read_only.next_hop = next_hop.map(|nh| nh.parse().unwrap());

        let bundle = Bundle {
            bundle: bpv7_bundle,
            metadata,
        };
        (bundle, data.into())
    }

    fn make_filter(patterns: &[&str]) -> IpnLegacyFilter {
        IpnLegacyFilter::new(make_config(patterns).0)
    }

    // IPNF-06b: No next-hop — no rewrite.
    #[tokio::test]
    async fn test_no_next_hop() {
        let filter = make_filter(&["ipn:*.*"]);
        let (bundle, data) = make_bundle("ipn:1.1.1", "ipn:1.2.1", None);

        let result = filter.filter(&bundle, &data).await.unwrap();
        assert!(
            matches!(result, WriteResult::Continue(None, None)),
            "No next-hop should mean no rewrite"
        );
    }

    // IPNF-06c: DTN source and destination — no rewrite even with matching next-hop.
    #[tokio::test]
    async fn test_dtn_no_rewrite() {
        let filter = make_filter(&["ipn:*.*"]);
        let (bundle, data) = make_bundle("dtn://node-a/svc", "dtn://node-b/svc", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        assert!(
            matches!(result, WriteResult::Continue(None, None)),
            "DTN EIDs should not be rewritten"
        );
    }

    // IPNF-01: allocator_id=0, non-matching next-hop — no rewrite.
    #[tokio::test]
    async fn test_alloc0_non_matching() {
        let filter = make_filter(&["ipn:0.99.*"]);
        let (bundle, data) = make_bundle("ipn:0.1.1", "ipn:0.2.1", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        assert!(
            matches!(result, WriteResult::Continue(None, None)),
            "Non-matching next-hop should mean no rewrite"
        );
    }

    // IPNF-02: allocator_id=0, matching next-hop — filter runs but bytes
    // are unchanged because the Builder already uses legacy 2-element
    // encoding when allocator_id=0.
    #[tokio::test]
    async fn test_alloc0_matching() {
        let filter = make_filter(&["ipn:*.*"]);
        let (bundle, data) = make_bundle("ipn:0.1.1", "ipn:0.2.1", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        let WriteResult::Continue(None, Some(new_data)) = result else {
            panic!("Expected rewrite path, got {result:?}");
        };

        assert_eq!(
            data,
            new_data.as_slice(),
            "allocator_id=0: rewrite should be idempotent"
        );
    }

    // IPNF-03: allocator_id!=0, non-matching next-hop — no rewrite.
    #[tokio::test]
    async fn test_alloc1_non_matching() {
        let filter = make_filter(&["ipn:0.99.*"]);
        let (bundle, data) = make_bundle("ipn:1.1.1", "ipn:1.2.1", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        assert!(
            matches!(result, WriteResult::Continue(None, None)),
            "Non-matching next-hop should mean no rewrite"
        );
    }

    // IPNF-04: allocator_id!=0, matching next-hop — bytes change because
    // 3-element [2, [1, 1, 1]] is rewritten to legacy 2-element.
    #[tokio::test]
    async fn test_alloc1_matching() {
        let filter = make_filter(&["ipn:*.*"]);
        let (bundle, data) = make_bundle("ipn:1.1.1", "ipn:1.2.1", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        let WriteResult::Continue(None, Some(new_data)) = result else {
            panic!("Expected rewrite path, got {result:?}");
        };

        assert_ne!(
            data,
            new_data.as_slice(),
            "allocator_id!=0: 3-element should be rewritten to 2-element"
        );

        let parsed = ParsedBundle::parse(&new_data, hardy_bpv7::bpsec::no_keys).unwrap();

        assert!(
            matches!(parsed.bundle.id.source, Eid::LegacyIpn { .. }),
            "Source should be LegacyIpn, got {:?}",
            parsed.bundle.id.source
        );
        assert!(
            matches!(parsed.bundle.destination, Eid::LegacyIpn { .. }),
            "Destination should be LegacyIpn, got {:?}",
            parsed.bundle.destination
        );
    }
}
