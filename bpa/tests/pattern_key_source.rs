//! PatternKeySource tests
//!
//! Verifies EID-pattern-based key selection, specificity ordering,
//! and operation routing via key_ops.

use std::collections::HashMap;

use hardy_bpa::key::pattern::{PatternKeySource, SecurityRole};
use hardy_bpv7::bpsec::key::{EncAlgorithm, Key, KeyAlgorithm, KeySource, Operation, Type, Use};
use hardy_bpv7::eid::Eid;
use hardy_eid_patterns::EidPattern;

fn hmac_key(kid: &str) -> Key {
    Key {
        id: Some(kid.into()),
        key_type: Type::OctetSequence {
            key: vec![0xAA; 32].into(),
        },
        key_algorithm: Some(KeyAlgorithm::HS256),
        operations: Some([Operation::Sign, Operation::Verify].into()),
        key_use: Some(Use::Signature),
        ..Default::default()
    }
}

fn aes_key(kid: &str) -> Key {
    Key {
        id: Some(kid.into()),
        key_type: Type::OctetSequence {
            key: vec![0xBB; 32].into(),
        },
        key_algorithm: Some(KeyAlgorithm::A256KW),
        enc_algorithm: Some(EncAlgorithm::A256GCM),
        operations: Some(
            [
                Operation::Encrypt,
                Operation::Decrypt,
                Operation::WrapKey,
                Operation::UnwrapKey,
            ]
            .into(),
        ),
        key_use: Some(Use::Encryption),
    }
}

fn keys(entries: &[(&str, Key)]) -> HashMap<String, Key> {
    entries
        .iter()
        .map(|(kid, key)| (kid.to_string(), key.clone()))
        .collect()
}

fn parse_eid(s: &str) -> Eid {
    s.parse().expect("valid EID")
}

fn parse_pattern(s: &str) -> EidPattern {
    s.parse().expect("valid EID pattern")
}

#[test]
fn no_policies_returns_none() {
    let source = PatternKeySource::new(keys(&[("k", hmac_key("k"))]), vec![]);

    assert!(
        source
            .key(&parse_eid("ipn:1.0"), &[Operation::Verify])
            .is_none()
    );
}

#[test]
fn no_matching_policy_returns_none() {
    let source = PatternKeySource::new(
        keys(&[("k", hmac_key("k"))]),
        vec![(
            parse_pattern("ipn:0.99.*"),
            SecurityRole::Acceptor,
            vec!["k".into()],
        )],
    );

    assert!(
        source
            .key(&parse_eid("ipn:0.1.0"), &[Operation::Verify])
            .is_none()
    );
}

#[test]
fn wildcard_matches_any() {
    let source = PatternKeySource::new(
        keys(&[("fleet", hmac_key("fleet"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            vec!["fleet".into()],
        )],
    );

    let key = source
        .key(&parse_eid("ipn:0.42.0"), &[Operation::Verify])
        .expect("should match wildcard");
    assert_eq!(key.id.as_deref(), Some("fleet"));
}

#[test]
fn specific_pattern_overrides_wildcard() {
    let source = PatternKeySource::new(
        keys(&[("fleet", hmac_key("fleet")), ("node42", hmac_key("node42"))]),
        vec![
            (
                parse_pattern("ipn:*.*"),
                SecurityRole::Acceptor,
                vec!["fleet".into()],
            ),
            (
                parse_pattern("ipn:0.42.*"),
                SecurityRole::Acceptor,
                vec!["node42".into()],
            ),
        ],
    );

    let key = source
        .key(&parse_eid("ipn:0.42.0"), &[Operation::Verify])
        .expect("should match node42");
    assert_eq!(key.id.as_deref(), Some("node42"));

    let key = source
        .key(&parse_eid("ipn:0.1.0"), &[Operation::Verify])
        .expect("should match fleet");
    assert_eq!(key.id.as_deref(), Some("fleet"));
}

#[test]
fn operation_routing_via_key_ops() {
    // Both keys bound to the same pattern; key_ops determines selection
    let source = PatternKeySource::new(
        keys(&[("hmac", hmac_key("hmac")), ("aes", aes_key("aes"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            vec!["hmac".into(), "aes".into()],
        )],
    );

    let eid = parse_eid("ipn:0.1.0");

    // Verify -> hmac (key_ops: sign, verify)
    let key = source.key(&eid, &[Operation::Verify]).unwrap();
    assert_eq!(key.id.as_deref(), Some("hmac"));

    // Sign -> hmac
    let key = source.key(&eid, &[Operation::Sign]).unwrap();
    assert_eq!(key.id.as_deref(), Some("hmac"));

    // Decrypt -> aes (key_ops: encrypt, decrypt, wrapKey, unwrapKey)
    let key = source.key(&eid, &[Operation::Decrypt]).unwrap();
    assert_eq!(key.id.as_deref(), Some("aes"));

    // Encrypt -> aes
    let key = source.key(&eid, &[Operation::Encrypt]).unwrap();
    assert_eq!(key.id.as_deref(), Some("aes"));

    // WrapKey -> aes
    let key = source.key(&eid, &[Operation::WrapKey]).unwrap();
    assert_eq!(key.id.as_deref(), Some("aes"));
}

#[test]
fn integrity_only_binding() {
    // Only an HMAC key bound: no confidentiality key available
    let source = PatternKeySource::new(
        keys(&[("hmac", hmac_key("hmac"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            vec!["hmac".into()],
        )],
    );

    let eid = parse_eid("ipn:0.1.0");

    assert!(source.key(&eid, &[Operation::Verify]).is_some());
    assert!(source.key(&eid, &[Operation::Decrypt]).is_none());
}

#[test]
fn missing_kid_reference_returns_none() {
    let source = PatternKeySource::new(
        keys(&[("real-key", hmac_key("real-key"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            vec!["nonexistent".into()],
        )],
    );

    assert!(
        source
            .key(&parse_eid("ipn:0.1.0"), &[Operation::Verify])
            .is_none()
    );
}

#[test]
fn key_order_determines_priority() {
    // Both keys support Verify, but hmac is listed first
    let source = PatternKeySource::new(
        keys(&[("hmac", hmac_key("hmac")), ("aes", aes_key("aes"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            vec!["hmac".into(), "aes".into()],
        )],
    );

    let eid = parse_eid("ipn:0.1.0");

    // Verify matches hmac first (listed first, has Verify in key_ops)
    let key = source.key(&eid, &[Operation::Verify]).unwrap();
    assert_eq!(key.id.as_deref(), Some("hmac"));
}

#[test]
fn add_and_remove_key() {
    let mut source = PatternKeySource::empty();
    source.add_key("k1".into(), hmac_key("k1"));
    source.add_binding(
        parse_pattern("ipn:*.*"),
        SecurityRole::Verifier,
        vec!["k1".into()],
    );

    let eid = parse_eid("ipn:0.1.0");
    assert!(source.key(&eid, &[Operation::Verify]).is_some());

    source.remove_key("k1");
    assert!(source.key(&eid, &[Operation::Verify]).is_none());
}

#[test]
fn add_and_remove_binding() {
    let mut source = PatternKeySource::empty();
    source.add_key("k1".into(), hmac_key("k1"));

    let pattern = parse_pattern("ipn:0.42.*");
    source.add_binding(pattern.clone(), SecurityRole::Verifier, vec!["k1".into()]);

    let eid = parse_eid("ipn:0.42.1");
    assert!(source.key(&eid, &[Operation::Verify]).is_some());

    source.remove_binding(&pattern);
    assert!(source.key(&eid, &[Operation::Verify]).is_none());
}
