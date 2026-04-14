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
    pub Vec<hardy_eid_patterns::EidPattern>,
);

/// Egress WriteFilter that rewrites IPN 3-element EIDs to legacy 2-element format.
///
/// # Example
///
/// ```ignore
/// let filter = IpnLegacyFilter::new(&config);
/// bpa.register_filter(
///     hardy_bpa::filters::Hook::Egress,
///     "ipn-legacy",
///     &[],
///     hardy_bpa::filters::Filter::Write(filter),
/// )?;
/// ```
pub struct IpnLegacyFilter {
    peer_patterns: Vec<hardy_eid_patterns::EidPattern>,
}

impl IpnLegacyFilter {
    /// Create a new IPN legacy encoding filter.
    ///
    /// Returns `None` if the config has no peer patterns (filter not needed).
    pub fn new(config: &Config) -> Option<Arc<Self>> {
        if config.0.is_empty() {
            None
        } else {
            Some(Arc::new(Self {
                peer_patterns: config.0.clone(),
            }))
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use hardy_bpa::bundle::{Bundle, BundleMetadata};
    use hardy_bpa::filters::{RewriteResult, WriteFilter};
    use hardy_bpv7::eid::Eid;

    fn make_config(patterns: &[&str]) -> Config {
        Config(patterns.iter().map(|p| p.parse().unwrap()).collect())
    }

    fn make_bundle(source: &str, dest: &str, next_hop: Option<&str>) -> (Bundle, Vec<u8>) {
        let src: Eid = source.parse().unwrap();
        let dst: Eid = dest.parse().unwrap();

        let (bpv7_bundle, data) = hardy_bpv7::builder::Builder::new(src, dst)
            .with_payload(std::borrow::Cow::Borrowed(b"test"))
            .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
            .unwrap();

        let mut metadata = BundleMetadata::default();
        metadata.read_only.next_hop = next_hop.map(|nh| nh.parse().unwrap());

        let bundle = Bundle {
            bundle: bpv7_bundle,
            metadata,
        };
        (bundle, data.into())
    }

    /// IPNF-06: Empty config returns None (filter not needed).
    #[test]
    fn test_empty_config() {
        let config = Config::default();
        assert!(IpnLegacyFilter::new(&config).is_none());
    }

    /// IPNF-06b: No next-hop — no rewrite.
    #[tokio::test]
    async fn test_no_next_hop() {
        let filter = IpnLegacyFilter::new(&make_config(&["ipn:*.*"])).unwrap();
        let (bundle, data) = make_bundle("ipn:1.1.1", "ipn:1.2.1", None);

        let result = filter.filter(&bundle, &data).await.unwrap();
        assert!(
            matches!(result, RewriteResult::Continue(None, None)),
            "No next-hop should mean no rewrite"
        );
    }

    /// IPNF-06c: DTN source and destination — no rewrite even with matching next-hop.
    #[tokio::test]
    async fn test_dtn_no_rewrite() {
        let filter = IpnLegacyFilter::new(&make_config(&["ipn:*.*"])).unwrap();
        let (bundle, data) = make_bundle("dtn://node-a/svc", "dtn://node-b/svc", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        assert!(
            matches!(result, RewriteResult::Continue(None, None)),
            "DTN EIDs should not be rewritten"
        );
    }

    // -------------------------------------------------------------------
    // Core 4 tests: allocator_id × matching/non-matching
    // -------------------------------------------------------------------

    /// IPNF-01: allocator_id=0, non-matching next-hop — no rewrite.
    #[tokio::test]
    async fn test_alloc0_non_matching() {
        let filter = IpnLegacyFilter::new(&make_config(&["ipn:0.99.*"])).unwrap();
        let (bundle, data) = make_bundle("ipn:0.1.1", "ipn:0.2.1", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        assert!(
            matches!(result, RewriteResult::Continue(None, None)),
            "Non-matching next-hop should mean no rewrite"
        );
    }

    /// IPNF-02: allocator_id=0, matching next-hop — filter runs but bytes
    /// are unchanged because the Builder already uses legacy 2-element
    /// encoding when allocator_id=0.
    #[tokio::test]
    async fn test_alloc0_matching() {
        let filter = IpnLegacyFilter::new(&make_config(&["ipn:*.*"])).unwrap();
        let (bundle, data) = make_bundle("ipn:0.1.1", "ipn:0.2.1", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        let RewriteResult::Continue(None, Some(new_data)) = result else {
            panic!("Expected rewrite path, got {result:?}");
        };

        // With allocator_id=0, Builder already produces legacy encoding,
        // so the output should be identical (idempotent rewrite).
        assert_eq!(
            data,
            new_data.as_ref(),
            "allocator_id=0: rewrite should be idempotent"
        );
    }

    /// IPNF-03: allocator_id!=0, non-matching next-hop — no rewrite.
    #[tokio::test]
    async fn test_alloc1_non_matching() {
        let filter = IpnLegacyFilter::new(&make_config(&["ipn:0.99.*"])).unwrap();
        let (bundle, data) = make_bundle("ipn:1.1.1", "ipn:1.2.1", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        assert!(
            matches!(result, RewriteResult::Continue(None, None)),
            "Non-matching next-hop should mean no rewrite"
        );
    }

    /// IPNF-04: allocator_id!=0, matching next-hop — bytes change because
    /// 3-element [2, [1, 1, 1]] is rewritten to legacy 2-element.
    #[tokio::test]
    async fn test_alloc1_matching() {
        let filter = IpnLegacyFilter::new(&make_config(&["ipn:*.*"])).unwrap();
        let (bundle, data) = make_bundle("ipn:1.1.1", "ipn:1.2.1", Some("ipn:0.3.0"));

        let result = filter.filter(&bundle, &data).await.unwrap();
        let RewriteResult::Continue(None, Some(new_data)) = result else {
            panic!("Expected rewrite path, got {result:?}");
        };

        // Wire format should change: 3-element → 2-element encoding
        assert_ne!(
            data,
            new_data.as_ref(),
            "allocator_id!=0: 3-element should be rewritten to 2-element"
        );

        // Verify the output is a valid bundle with legacy EIDs
        let parsed =
            hardy_bpv7::bundle::ParsedBundle::parse(&new_data, hardy_bpv7::bpsec::no_keys).unwrap();

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
