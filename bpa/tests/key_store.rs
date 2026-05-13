//! KeyStore unit tests
//!
//! Verifies add/remove, snapshot isolation, and composite lookup behaviour.

use std::sync::Arc;

use hardy_bpa::key_store::KeyStore;
use hardy_bpv7::bpsec::key::{Key, KeySource, Operation, Type};
use hardy_bpv7::eid::Eid;

/// A trivial KeySource that always returns the same key.
struct StaticKeySource {
    key: Key,
}

impl KeySource for StaticKeySource {
    fn key(&self, _source: &Eid, _operations: &[Operation]) -> Option<&Key> {
        Some(&self.key)
    }
}

fn make_key(id: &str) -> Key {
    Key {
        id: Some(id.into()),
        key_type: Type::OctetSequence {
            key: vec![1, 2, 3].into(),
        },
        operations: Some([Operation::Verify].into()),
        ..Default::default()
    }
}

fn make_source(id: &str) -> Arc<dyn KeySource> {
    Arc::new(StaticKeySource { key: make_key(id) })
}

#[test]
fn empty_store_returns_none() {
    let store = KeyStore::new();
    let keys = store.current();
    assert!(keys.key(&Eid::default(), &[Operation::Verify]).is_none());
}

#[test]
fn add_source_then_lookup() {
    let store = KeyStore::new();
    store.add("test".into(), make_source("key-1"));

    let keys = store.current();
    let key = keys.key(&Eid::default(), &[Operation::Verify]);
    assert!(key.is_some());
    assert_eq!(key.unwrap().id.as_deref(), Some("key-1"));
}

#[test]
fn remove_source() {
    let store = KeyStore::new();
    store.add("test".into(), make_source("key-1"));
    store.remove("test");

    let keys = store.current();
    assert!(keys.key(&Eid::default(), &[Operation::Verify]).is_none());
}

#[test]
fn remove_nonexistent_returns_none() {
    let store = KeyStore::new();
    assert!(store.remove("nonexistent").is_none());
}

#[test]
fn add_replaces_existing() {
    let store = KeyStore::new();
    store.add("test".into(), make_source("key-1"));
    let old = store.add("test".into(), make_source("key-2"));
    assert!(old.is_some());

    let keys = store.current();
    let key = keys.key(&Eid::default(), &[Operation::Verify]).unwrap();
    assert_eq!(key.id.as_deref(), Some("key-2"));
}

#[test]
fn snapshot_isolation() {
    let store = KeyStore::new();
    store.add("test".into(), make_source("key-1"));

    // Take a snapshot
    let snapshot = store.current();

    // Modify the store
    store.remove("test");

    // Old snapshot still works
    assert!(
        snapshot
            .key(&Eid::default(), &[Operation::Verify])
            .is_some()
    );

    // New snapshot reflects the change
    let new_snapshot = store.current();
    assert!(
        new_snapshot
            .key(&Eid::default(), &[Operation::Verify])
            .is_none()
    );
}

#[test]
fn multiple_sources_returns_a_match() {
    let store = KeyStore::new();
    store.add("a".into(), make_source("key-a"));
    store.add("b".into(), make_source("key-b"));

    let keys = store.current();
    let key = keys.key(&Eid::default(), &[Operation::Verify]).unwrap();
    // HashMap ordering is not guaranteed, either key is valid
    assert!(key.id.as_deref() == Some("key-a") || key.id.as_deref() == Some("key-b"));
}
