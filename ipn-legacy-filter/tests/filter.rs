use hardy_bpa::{
    bundle::{Bundle, BundleMetadata},
    filter::{WriteFilter, WriteResult},
};
use hardy_bpv7::{
    builder::Builder, bundle::ParsedBundle, creation_timestamp::CreationTimestamp, eid::Eid,
};
use hardy_ipn_legacy_filter::{Config, IpnLegacyFilter};

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

// IPNF-05: IPN source + DTN destination — only the source is rewritten,
// the destination and payload are untouched.
#[tokio::test]
async fn test_mixed_source_only_rewrite() {
    let filter = make_filter(&["ipn:*.*"]);
    let (bundle, data) = make_bundle("ipn:1.1.1", "dtn://node-b/svc", Some("ipn:0.3.0"));

    let result = filter.filter(&bundle, &data).await.unwrap();
    let WriteResult::Continue(None, Some(new_data)) = result else {
        panic!("Expected rewrite path, got {result:?}");
    };

    let parsed = ParsedBundle::parse(&new_data, hardy_bpv7::bpsec::no_keys).unwrap();
    assert!(
        matches!(parsed.bundle.id.source, Eid::LegacyIpn { .. }),
        "Source should be LegacyIpn, got {:?}",
        parsed.bundle.id.source
    );
    assert!(
        matches!(parsed.bundle.destination, Eid::Dtn { .. }),
        "Destination should stay Dtn, got {:?}",
        parsed.bundle.destination
    );
    assert_eq!(
        parsed
            .bundle
            .blocks
            .get(&1)
            .and_then(|block| block.payload(&new_data)),
        Some(b"test".as_slice()),
        "Payload must be intact after the rewrite"
    );
}

// IPNF-05 mirror: DTN source + IPN destination — only the destination is
// rewritten.
#[tokio::test]
async fn test_mixed_dest_only_rewrite() {
    let filter = make_filter(&["ipn:*.*"]);
    let (bundle, data) = make_bundle("dtn://node-a/svc", "ipn:1.2.1", Some("ipn:0.3.0"));

    let result = filter.filter(&bundle, &data).await.unwrap();
    let WriteResult::Continue(None, Some(new_data)) = result else {
        panic!("Expected rewrite path, got {result:?}");
    };

    let parsed = ParsedBundle::parse(&new_data, hardy_bpv7::bpsec::no_keys).unwrap();
    assert!(
        matches!(parsed.bundle.id.source, Eid::Dtn { .. }),
        "Source should stay Dtn, got {:?}",
        parsed.bundle.id.source
    );
    assert!(
        matches!(parsed.bundle.destination, Eid::LegacyIpn { .. }),
        "Destination should be LegacyIpn, got {:?}",
        parsed.bundle.destination
    );
}

// IPNF-06: an empty pattern list is a no-op for any bundle (`.any` on an
// empty iterator is false, never true).
#[tokio::test]
async fn test_empty_config() {
    let filter = make_filter(&[]);
    let (bundle, data) = make_bundle("ipn:1.1.1", "ipn:1.2.1", Some("ipn:0.3.0"));

    let result = filter.filter(&bundle, &data).await.unwrap();
    assert!(
        matches!(result, WriteResult::Continue(None, None)),
        "An empty filter should never rewrite"
    );
}
