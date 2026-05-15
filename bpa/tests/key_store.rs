//! KeyStore unit tests
//!
//! Verifies set, snapshot isolation, and default (empty) behaviour.

use std::collections::HashMap;
use std::sync::Arc;

use hardy_bpa::security::KeyStore;
use hardy_bpa::security::pattern::{PatternKeySource, SecurityRole};
use hardy_bpv7::bpsec::key::{Key, KeySource, Operation, Type};
use hardy_bpv7::eid::Eid;

fn make_source(kid: &str) -> Arc<PatternKeySource> {
    let key = Key {
        id: Some(kid.into()),
        key_type: Type::OctetSequence {
            key: vec![1, 2, 3].into(),
        },
        operations: Some([Operation::Verify].into()),
        ..Default::default()
    };
    let mut keys = HashMap::new();
    keys.insert(kid.to_string(), key);
    Arc::new(PatternKeySource::new(
        keys,
        vec![(
            "ipn:*.*".parse().unwrap(),
            SecurityRole::Acceptor,
            Some(kid.into()),
            None,
        )],
    ))
}

#[test]
fn empty_store_returns_none() {
    let store = KeyStore::new();
    let keys = store.current();
    assert!(keys.key(&Eid::default(), &[Operation::Verify]).is_none());
}

#[test]
fn set_source_then_lookup() {
    let store = KeyStore::new();
    store.set(make_source("key-1"));

    let keys = store.current();
    let eid: Eid = "ipn:0.1.0".parse().unwrap();
    let key = keys.key(&eid, &[Operation::Verify]);
    assert!(key.is_some());
    assert_eq!(key.unwrap().id.as_deref(), Some("key-1"));
}

#[test]
fn set_replaces_previous() {
    let store = KeyStore::new();
    store.set(make_source("key-1"));
    store.set(make_source("key-2"));

    let keys = store.current();
    let eid: Eid = "ipn:0.1.0".parse().unwrap();
    let key = keys.key(&eid, &[Operation::Verify]).unwrap();
    assert_eq!(key.id.as_deref(), Some("key-2"));
}

#[test]
fn snapshot_isolation() {
    let store = KeyStore::new();
    store.set(make_source("key-1"));

    // Take a snapshot
    let snapshot = store.current();

    // Replace the source
    store.set(make_source("key-2"));

    let eid: Eid = "ipn:0.1.0".parse().unwrap();

    // Old snapshot still returns key-1
    let key = snapshot.key(&eid, &[Operation::Verify]).unwrap();
    assert_eq!(key.id.as_deref(), Some("key-1"));

    // New snapshot returns key-2
    let new_snapshot = store.current();
    let key = new_snapshot.key(&eid, &[Operation::Verify]).unwrap();
    assert_eq!(key.id.as_deref(), Some("key-2"));
}
