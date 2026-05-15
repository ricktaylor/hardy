//! PatternKeySource tests
//!
//! Verifies EID-pattern-based key selection, specificity ordering,
//! and operation-type routing (integrity vs confidentiality).

use std::collections::HashMap;

use hardy_bpa::security::pattern::{PatternKeySource, SecurityRole};
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
            Some("k".into()),
            None,
        )],
    );

    // ipn:0.1.0 doesn't match ipn:0.99.*
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
            Some("fleet".into()),
            None,
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
            // Wildcard — less specific
            (
                parse_pattern("ipn:*.*"),
                SecurityRole::Acceptor,
                Some("fleet".into()),
                None,
            ),
            // Exact node — more specific
            (
                parse_pattern("ipn:0.42.*"),
                SecurityRole::Acceptor,
                Some("node42".into()),
                None,
            ),
        ],
    );

    // Node 42 should get the specific key
    let key = source
        .key(&parse_eid("ipn:0.42.0"), &[Operation::Verify])
        .expect("should match node42");
    assert_eq!(key.id.as_deref(), Some("node42"));

    // Other nodes should get the fleet key
    let key = source
        .key(&parse_eid("ipn:0.1.0"), &[Operation::Verify])
        .expect("should match fleet");
    assert_eq!(key.id.as_deref(), Some("fleet"));
}

#[test]
fn integrity_vs_confidentiality_routing() {
    let source = PatternKeySource::new(
        keys(&[("hmac", hmac_key("hmac")), ("aes", aes_key("aes"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            Some("hmac".into()),
            Some("aes".into()),
        )],
    );

    let eid = parse_eid("ipn:0.1.0");

    // Verify → integrity key
    let key = source.key(&eid, &[Operation::Verify]).unwrap();
    assert_eq!(key.id.as_deref(), Some("hmac"));

    // Sign → integrity key
    let key = source.key(&eid, &[Operation::Sign]).unwrap();
    assert_eq!(key.id.as_deref(), Some("hmac"));

    // Decrypt → confidentiality key
    let key = source.key(&eid, &[Operation::Decrypt]).unwrap();
    assert_eq!(key.id.as_deref(), Some("aes"));

    // Encrypt → confidentiality key
    let key = source.key(&eid, &[Operation::Encrypt]).unwrap();
    assert_eq!(key.id.as_deref(), Some("aes"));

    // WrapKey → confidentiality key
    let key = source.key(&eid, &[Operation::WrapKey]).unwrap();
    assert_eq!(key.id.as_deref(), Some("aes"));

    // UnwrapKey → confidentiality key
    let key = source.key(&eid, &[Operation::UnwrapKey]).unwrap();
    assert_eq!(key.id.as_deref(), Some("aes"));
}

#[test]
fn integrity_only_policy() {
    let source = PatternKeySource::new(
        keys(&[("hmac", hmac_key("hmac"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            Some("hmac".into()),
            None,
        )],
    );

    let eid = parse_eid("ipn:0.1.0");

    // Verify works
    assert!(source.key(&eid, &[Operation::Verify]).is_some());

    // Decrypt returns None — no confidentiality key configured
    assert!(source.key(&eid, &[Operation::Decrypt]).is_none());
}

#[test]
fn missing_kid_reference_returns_none() {
    // Policy references "nonexistent" which isn't in the key map
    let source = PatternKeySource::new(
        keys(&[("real-key", hmac_key("real-key"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            Some("nonexistent".into()),
            None,
        )],
    );

    assert!(
        source
            .key(&parse_eid("ipn:0.1.0"), &[Operation::Verify])
            .is_none()
    );
}

#[test]
fn mixed_operations_returns_first_matching() {
    let source = PatternKeySource::new(
        keys(&[("hmac", hmac_key("hmac")), ("aes", aes_key("aes"))]),
        vec![(
            parse_pattern("ipn:*.*"),
            SecurityRole::Acceptor,
            Some("hmac".into()),
            Some("aes".into()),
        )],
    );

    let eid = parse_eid("ipn:0.1.0");

    // [UnwrapKey, Verify] — UnwrapKey matches confidentiality first
    let key = source
        .key(&eid, &[Operation::UnwrapKey, Operation::Verify])
        .unwrap();
    assert_eq!(key.id.as_deref(), Some("aes"));

    // [Verify, UnwrapKey] — Verify matches integrity first
    let key = source
        .key(&eid, &[Operation::Verify, Operation::UnwrapKey])
        .unwrap();
    assert_eq!(key.id.as_deref(), Some("hmac"));
}
