//! Bundle creation-time and expiry semantics through the public `bundle` API.

use hardy_bpa::bundle::Bundle;
use hardy_bpv7::{bundle::Bundle as Bpv7Bundle, creation_timestamp::CreationTimestamp};
use time::Duration;

fn make_bundle(
    timestamp: CreationTimestamp,
    age: Option<core::time::Duration>,
    lifetime: core::time::Duration,
) -> Bundle {
    Bundle {
        bundle: Bpv7Bundle {
            id: hardy_bpv7::bundle::Id {
                source: "ipn:0.99.1".parse().unwrap(),
                timestamp,
                fragment_info: None,
            },
            flags: Default::default(),
            crc_type: Default::default(),
            destination: "ipn:0.1.99".parse().unwrap(),
            report_to: Default::default(),
            lifetime,
            previous_node: None,
            age,
            hop_count: None,
            blocks: Default::default(),
        },
        metadata: Default::default(),
    }
}

// When creation timestamp is zero (unknown), creation_time() should
// fall back to received_at minus bundle age.
#[test]
fn test_age_fallback() {
    let age = core::time::Duration::from_secs(60);
    let bundle = make_bundle(
        CreationTimestamp::default(),
        Some(age),
        core::time::Duration::ZERO,
    );

    // With zero timestamp, creation_time = received_at - age
    let expected = bundle
        .metadata
        .read_only
        .received_at
        .saturating_sub(age.try_into().unwrap());
    let actual = bundle.creation_time();

    // Allow 1ms tolerance for test timing
    let diff = (actual - expected).abs();
    assert!(
        diff < Duration::milliseconds(1),
        "Age fallback: expected {expected}, got {actual}, diff {diff}"
    );
}

// Expiry = creation_time + lifetime
#[test]
fn test_expiry_calculation() {
    let lifetime = core::time::Duration::from_secs(3600);
    let bundle = make_bundle(CreationTimestamp::now(), None, lifetime);

    let creation = bundle.creation_time();
    let expiry = bundle.expiry();
    let diff = expiry - creation;

    // Should be exactly the lifetime (within 1ms tolerance)
    let expected = Duration::seconds(3600);
    assert!(
        (diff - expected).abs() < Duration::milliseconds(1),
        "Expiry should be creation + lifetime, got diff={diff}"
    );
}
