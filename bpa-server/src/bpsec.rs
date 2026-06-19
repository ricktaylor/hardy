//! BPSec key policy: EID-pattern key bindings with role-gated key release.
//!
//! Implements [`hardy_bpa::keys::KeyProvider`] over an atomically swappable
//! [`PatternKeySource`], so the key configuration can be hot-reloaded while
//! bundles are being processed.

use std::{collections::HashMap, path::Path, sync::Arc};

use arc_swap::ArcSwap;
use hardy_async::{TaskPool, watcher};
use hardy_bpv7::{
    bpsec::key::{Key, KeySet, KeySource, Operation, Type},
    eid::Eid,
};
use hardy_eid_patterns::EidPattern;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

/// The BPA's role with respect to a security block (RFC 9172 Section 2.5).
///
/// A role is expressed entirely through which operations keys are released
/// for: releasing a key claims responsibility for the operation (a failure
/// with a released key indicates corruption, RFC 9172 Section 5.1.1), while
/// withholding one produces `NoKey` and the security block is forwarded
/// intact for a downstream node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecurityRole {
    /// Verify BIBs in transit but keep them; BCBs ride through encrypted.
    #[default]
    Verifier,
    /// Verify BIBs and decrypt BCBs (decrypted targets are rewritten as
    /// plaintext and the BCB removed).
    Acceptor,
    /// Release keys only for adding protection at egress.
    Source,
}

impl SecurityRole {
    /// Whether this role releases keys for the given operation.
    fn serves(&self, operation: &Operation) -> bool {
        match self {
            SecurityRole::Verifier => matches!(operation, Operation::Verify),
            SecurityRole::Acceptor => matches!(
                operation,
                Operation::Verify | Operation::Decrypt | Operation::UnwrapKey
            ),
            SecurityRole::Source => matches!(
                operation,
                Operation::Sign
                    | Operation::Encrypt
                    | Operation::WrapKey
                    | Operation::DeriveKey
                    | Operation::DeriveBits
            ),
        }
    }
}

/// Binds an EID pattern to a set of key identifiers and a security role.
///
/// Keys are not split by integrity/confidentiality: the BPSec backend
/// selects the right key by matching the required operation against each
/// key's `key_ops` field.
#[derive(Debug, Clone)]
struct KeyBinding {
    pattern: EidPattern,
    role: SecurityRole,
    kids: Vec<String>,
}

/// A [`KeySource`] that selects keys by matching security source EIDs
/// against EID patterns.
///
/// Bindings are sorted by specificity (most specific first). On lookup, the
/// first matching binding wins; the requested operations are gated by the
/// binding's [`SecurityRole`], then keys are filtered by matching the
/// surviving operations against each key's `key_ops`.
#[derive(Debug, Clone)]
pub struct PatternKeySource {
    keys: HashMap<String, Key>,
    bindings: Vec<KeyBinding>,
}

impl PatternKeySource {
    /// Creates a new `PatternKeySource` from pre-loaded keys and bindings.
    ///
    /// - `keys`: keys indexed by `kid`
    /// - `bindings`: `(pattern, role, key_ids)` tuples,
    ///   sorted by specificity (highest first)
    pub fn new(
        keys: HashMap<String, Key>,
        mut bindings: Vec<(EidPattern, SecurityRole, Vec<String>)>,
    ) -> Self {
        bindings.sort_by(|a, b| {
            let score_a = a.0.specificity_score().unwrap_or(0);
            let score_b = b.0.specificity_score().unwrap_or(0);
            score_b.cmp(&score_a)
        });

        let bindings = bindings
            .into_iter()
            .map(|(pattern, role, kids)| KeyBinding {
                pattern,
                role,
                kids,
            })
            .collect();

        Self { keys, bindings }
    }

    /// Creates an empty source with no keys or bindings.
    pub fn empty() -> Self {
        Self {
            keys: HashMap::new(),
            bindings: Vec::new(),
        }
    }

    /// Loads keys from the JWKS file named in `config` and builds a
    /// `PatternKeySource`.
    ///
    /// Every key must be a non-empty symmetric key (`kty: oct`) carrying a
    /// `key_ops` field, and every binding must reference a known key id.
    pub fn from_config(config: &crate::config::bpsec::Config) -> anyhow::Result<Self> {
        check_permissions(&config.keys_file);

        let file = std::fs::File::open(&config.keys_file).map_err(|e| {
            anyhow::anyhow!(
                "Failed to open key file '{}': {e}",
                config.keys_file.display()
            )
        })?;

        let key_set: KeySet = serde_json::from_reader(file).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse key file '{}': {e}",
                config.keys_file.display()
            )
        })?;

        // Index keys by kid with validation
        let mut keys = HashMap::new();
        for key in key_set.keys {
            let Some(kid) = key.id.clone() else {
                warn!("Key file contains a key without a 'kid' field, skipping");
                continue;
            };

            if !matches!(key.key_type, Type::OctetSequence { ref key } if !key.is_empty()) {
                anyhow::bail!("Key '{kid}' must be a non-empty symmetric key (kty: oct)");
            }

            if key.operations.is_none() {
                anyhow::bail!("Key '{kid}' has no 'key_ops' field, cannot match any operation");
            }

            if keys.insert(kid.clone(), key).is_some() {
                anyhow::bail!("Key file contains duplicate kid '{kid}'");
            }
        }

        // Validate bindings
        for binding in &config.bindings {
            if binding.keys.is_empty() {
                anyhow::bail!("Security binding for '{}' has no keys", binding.pattern);
            }

            for kid in &binding.keys {
                if !keys.contains_key(kid) {
                    anyhow::bail!("Security binding references unknown key id '{kid}'");
                }
            }
        }

        let bindings = config
            .bindings
            .iter()
            .map(|b| (b.pattern.clone(), b.role, b.keys.clone()))
            .collect();

        Ok(PatternKeySource::new(keys, bindings))
    }
}

#[cfg(unix)]
fn check_permissions(path: &Path) {
    use std::os::unix::fs::MetadataExt;

    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            warn!(
                "Key file '{}' has group/other permissions (mode {:04o}). \
                 Restrict to owner-only (chmod 0600).",
                path.display(),
                mode
            );
        }
    }
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) {}

impl KeySource for PatternKeySource {
    fn key<'a>(&'a self, source: &Eid, operations: &[Operation]) -> Option<&'a Key> {
        let binding = self.bindings.iter().find(|b| b.pattern.matches(source))?;

        // Gate the requested operations by the binding's role
        let served: Vec<&Operation> = operations
            .iter()
            .filter(|op| binding.role.serves(op))
            .collect();
        if served.is_empty() {
            debug!(
                "Role {:?} for {source} withholds keys for {operations:?}",
                binding.role
            );
            return None;
        }

        // Find the first bound key whose key_ops supports any served operation
        for kid in &binding.kids {
            if let Some(key) = self.keys.get(kid)
                && let Some(key_ops) = &key.operations
                && served.iter().any(|op| key_ops.contains(op))
            {
                return Some(key);
            }
        }
        None
    }
}

/// A [`hardy_bpa::keys::KeyProvider`] backed by an atomically swappable
/// [`PatternKeySource`].
///
/// [`set`](Self::set) replaces the source without locking, so configuration
/// can be hot-reloaded while bundles are being processed.
pub struct PatternKeyProvider {
    source: ArcSwap<PatternKeySource>,
}

impl PatternKeyProvider {
    pub fn new(source: PatternKeySource) -> Self {
        Self {
            source: ArcSwap::from_pointee(source),
        }
    }

    /// Replace the current key source.
    pub fn set(&self, source: PatternKeySource) {
        self.source.store(Arc::new(source));
    }
}

impl Default for PatternKeyProvider {
    fn default() -> Self {
        Self::new(PatternKeySource::empty())
    }
}

impl hardy_bpa::keys::KeyProvider for PatternKeyProvider {
    fn key_source(&self, _bundle: &hardy_bpv7::bundle::Bundle, _data: &[u8]) -> Box<dyn KeySource> {
        Box::new(CurrentKeys(self.source.load_full()))
    }
}

/// A point-in-time snapshot of a [`PatternKeyProvider`]'s source.
///
/// This provider keys off a single global table, so its snapshot shares the
/// current table via `Arc` rather than building a per-flow subset: that keeps
/// `key_source` O(1) on the per-bundle parse path and gives each bundle a view
/// stable across a hot-reload. The newtype only exists to impl [`KeySource`]
/// for the `Arc` (orphan rule).
struct CurrentKeys(Arc<PatternKeySource>);

impl KeySource for CurrentKeys {
    fn key<'a>(&'a self, source: &Eid, operations: &[Operation]) -> Option<&'a Key> {
        self.0.key(source, operations)
    }
}

/// Spawn a task that watches the configured key file and reloads it into
/// `provider` on change. A reload failure keeps the previous keys.
/// No-op if watching is disabled in the config.
pub fn watch_keys(
    tasks: &TaskPool,
    config: crate::config::bpsec::Config,
    provider: Arc<PatternKeyProvider>,
) {
    let Some(watch_mode) = config.watch.into() else {
        return;
    };

    let keys_file = config.keys_file.clone();
    let cancel = tasks.cancel_token().clone();
    hardy_async::spawn!(tasks, "key_file_watcher", async move {
        watcher::watch(&keys_file, watch_mode, cancel, move || {
            let config = config.clone();
            let provider = provider.clone();
            async move {
                info!("Key file changed, reloading");
                match PatternKeySource::from_config(&config) {
                    Ok(source) => {
                        provider.set(source);
                        info!("Keys reloaded successfully");
                    }
                    Err(e) => {
                        error!("Failed to reload keys: {e}. Keeping previous keys.");
                    }
                }
            }
        })
        .await;
    });
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use hardy_bpa::keys::KeyProvider;
    use hardy_bpv7::{
        block,
        bpsec::key::{EncAlgorithm, KeyAlgorithm, Type, Use},
        bundle::RewrittenBundle,
    };

    use super::*;
    use crate::config::{
        WatchConfig,
        bpsec::{Config, KeyBindingConfig},
    };

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

        // Decrypt -> aes (key_ops: encrypt, decrypt, wrapKey, unwrapKey)
        let key = source.key(&eid, &[Operation::Decrypt]).unwrap();
        assert_eq!(key.id.as_deref(), Some("aes"));

        // UnwrapKey -> aes
        let key = source.key(&eid, &[Operation::UnwrapKey]).unwrap();
        assert_eq!(key.id.as_deref(), Some("aes"));
    }

    #[test]
    fn verifier_role_withholds_decrypt_keys() {
        // A verifier releases keys only to verify: BCBs ride through encrypted
        let source = PatternKeySource::new(
            keys(&[("hmac", hmac_key("hmac")), ("aes", aes_key("aes"))]),
            vec![(
                parse_pattern("ipn:*.*"),
                SecurityRole::Verifier,
                vec!["hmac".into(), "aes".into()],
            )],
        );

        let eid = parse_eid("ipn:0.1.0");

        assert!(source.key(&eid, &[Operation::Verify]).is_some());
        assert!(source.key(&eid, &[Operation::Decrypt]).is_none());
        assert!(source.key(&eid, &[Operation::UnwrapKey]).is_none());
        assert!(source.key(&eid, &[Operation::Sign]).is_none());
    }

    #[test]
    fn source_role_releases_only_protection_keys() {
        let source = PatternKeySource::new(
            keys(&[("hmac", hmac_key("hmac")), ("aes", aes_key("aes"))]),
            vec![(
                parse_pattern("ipn:*.*"),
                SecurityRole::Source,
                vec!["hmac".into(), "aes".into()],
            )],
        );

        let eid = parse_eid("ipn:0.1.0");

        assert_eq!(
            source
                .key(&eid, &[Operation::Sign])
                .and_then(|k| k.id.as_deref()),
            Some("hmac")
        );
        assert_eq!(
            source
                .key(&eid, &[Operation::Encrypt])
                .and_then(|k| k.id.as_deref()),
            Some("aes")
        );
        assert_eq!(
            source
                .key(&eid, &[Operation::WrapKey])
                .and_then(|k| k.id.as_deref()),
            Some("aes")
        );
        assert!(source.key(&eid, &[Operation::Verify]).is_none());
        assert!(source.key(&eid, &[Operation::Decrypt]).is_none());
    }

    #[test]
    fn most_specific_role_wins() {
        // A specific verifier binding shadows the wildcard acceptor for its
        // EIDs: decrypt keys are withheld even though the wildcard would
        // release them
        let source = PatternKeySource::new(
            keys(&[("hmac", hmac_key("hmac")), ("aes", aes_key("aes"))]),
            vec![
                (
                    parse_pattern("ipn:*.*"),
                    SecurityRole::Acceptor,
                    vec!["hmac".into(), "aes".into()],
                ),
                (
                    parse_pattern("ipn:0.42.*"),
                    SecurityRole::Verifier,
                    vec!["hmac".into(), "aes".into()],
                ),
            ],
        );

        assert!(
            source
                .key(&parse_eid("ipn:0.42.0"), &[Operation::Decrypt])
                .is_none()
        );
        assert!(
            source
                .key(&parse_eid("ipn:0.1.0"), &[Operation::Decrypt])
                .is_some()
        );
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

    // RFC 9173 Appendix A.2 test vector: a BCB from security source ipn:2.1
    // protecting the payload. Payload BCBs ride through parse intact and are
    // decrypted at delivery via `block_data`; the role decides whether the
    // decrypt key is released there: a verifier gets NoKey, an acceptor gets
    // the plaintext.
    fn a2_source(role: SecurityRole) -> PatternKeySource {
        let key = Key {
            id: Some("a2-key".into()),
            key_type: Type::OctetSequence {
                key: b"abcdefghijklmnop".to_vec().into(),
            },
            key_algorithm: Some(KeyAlgorithm::A128KW),
            enc_algorithm: Some(EncAlgorithm::A128GCM),
            operations: Some([Operation::UnwrapKey, Operation::Decrypt].into()),
            ..Default::default()
        };
        PatternKeySource::new(
            keys(&[("a2-key", key)]),
            vec![(parse_pattern("ipn:2.*"), role, vec!["a2-key".into()])],
        )
    }

    const A2_BUNDLE: [u8; 159] = hex_literal::hex!(
        "9f88070000820282010282028202018202820201820018281a000f4240850c0201
                0058508101020182028202018482014c5477656c7665313231323132820201820358
                1869c411276fecddc4780df42c8a2af89296fabf34d7fae7008204008181820150ef
                a4b5ac0108e3816c5606479801bc04850101000058233a09c1e63fe23a7f66a59c73
                03837241e070b02619fc59c5214a22f08cd70795e73e9aff"
    );

    fn count_bcbs(bundle: &hardy_bpv7::bundle::Bundle) -> usize {
        bundle
            .blocks
            .values()
            .filter(|b| b.block_type == block::Type::BlockSecurity)
            .count()
    }

    #[test]
    fn verifier_forwards_bcb_intact() {
        let source = a2_source(SecurityRole::Verifier);
        let result = RewrittenBundle::parse_with_keys(&A2_BUNDLE, &source).expect("parse failed");

        let RewrittenBundle::Valid { bundle, .. } = result else {
            panic!("verifier must not rewrite the bundle");
        };
        assert_eq!(count_bcbs(&bundle), 1);

        // The decrypt key is withheld at delivery: NoKey, payload stays encrypted
        assert!(matches!(
            bundle.block_data(1, &A2_BUNDLE, &source),
            Err(hardy_bpv7::Error::InvalidBPSec(
                hardy_bpv7::bpsec::Error::NoKey
            ))
        ));
    }

    #[test]
    fn acceptor_decrypts_payload_at_delivery() {
        let source = a2_source(SecurityRole::Acceptor);
        let result = RewrittenBundle::parse_with_keys(&A2_BUNDLE, &source).expect("parse failed");

        let RewrittenBundle::Valid { bundle, .. } = result else {
            panic!("payload BCB decryption is deferred to delivery, not parse");
        };
        assert_eq!(count_bcbs(&bundle), 1);

        let payload = bundle
            .block_data(1, &A2_BUNDLE, &source)
            .expect("acceptor must decrypt the payload");
        assert_eq!(payload.as_ref(), b"Ready to generate a 32-byte payload");
    }

    fn make_source(kid: &str) -> PatternKeySource {
        let key = Key {
            id: Some(kid.into()),
            key_type: Type::OctetSequence {
                key: vec![1, 2, 3].into(),
            },
            operations: Some([Operation::Verify].into()),
            ..Default::default()
        };
        PatternKeySource::new(
            keys(&[(kid, key)]),
            vec![(
                parse_pattern("ipn:*.*"),
                SecurityRole::Acceptor,
                vec![kid.into()],
            )],
        )
    }

    #[test]
    fn empty_provider_returns_none() {
        let provider = PatternKeyProvider::default();
        let keys = provider.key_source(&Default::default(), &[]);
        assert!(keys.key(&Eid::default(), &[Operation::Verify]).is_none());
    }

    #[test]
    fn set_replaces_previous() {
        let provider = PatternKeyProvider::new(make_source("key-1"));
        provider.set(make_source("key-2"));

        let keys = provider.key_source(&Default::default(), &[]);
        let key = keys
            .key(&parse_eid("ipn:0.1.0"), &[Operation::Verify])
            .unwrap();
        assert_eq!(key.id.as_deref(), Some("key-2"));
    }

    #[test]
    fn snapshot_isolation() {
        let provider = PatternKeyProvider::new(make_source("key-1"));

        // The KeySource handed out for a bundle is a point-in-time snapshot
        let snapshot = provider.key_source(&Default::default(), &[]);

        provider.set(make_source("key-2"));

        let eid = parse_eid("ipn:0.1.0");

        // Old snapshot still returns key-1
        let key = snapshot.key(&eid, &[Operation::Verify]).unwrap();
        assert_eq!(key.id.as_deref(), Some("key-1"));

        // A fresh snapshot returns key-2
        let new_snapshot = provider.key_source(&Default::default(), &[]);
        let key = new_snapshot.key(&eid, &[Operation::Verify]).unwrap();
        assert_eq!(key.id.as_deref(), Some("key-2"));
    }

    fn write_keys(dir: &tempfile::TempDir, json: &str) -> PathBuf {
        let path = dir.path().join("keys.jwks");
        std::fs::write(&path, json).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn config_with(keys_path: &Path, bindings: Vec<KeyBindingConfig>) -> Config {
        Config {
            keys_file: keys_path.to_path_buf(),
            watch: WatchConfig::None,
            bindings,
        }
    }

    fn binding(pattern: &str, role: SecurityRole, keys: &[&str]) -> KeyBindingConfig {
        KeyBindingConfig {
            pattern: pattern.parse().unwrap(),
            role,
            keys: keys.iter().map(|s| s.to_string()).collect(),
        }
    }

    const VALID_KEYS: &str = r#"{
        "keys": [
            {
                "kid": "hmac-key",
                "kty": "oct",
                "k": "hJtXIZ2uSN5kbQfbtTNWbpdmhkV8FJG-Onbc6mxCcYg",
                "alg": "HS256",
                "key_ops": ["sign", "verify"]
            },
            {
                "kid": "aes-key",
                "kty": "oct",
                "k": "hJtXIZ2uSN5kbQfbtTNWbpdmhkV8FJG-Onbc6mxCcYg",
                "alg": "A256KW",
                "enc": "A256GCM",
                "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"]
            }
        ]
    }"#;

    #[test]
    fn build_succeeds_with_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(&dir, VALID_KEYS);
        let config = config_with(
            &keys_path,
            vec![binding(
                "ipn:*.*",
                SecurityRole::Acceptor,
                &["hmac-key", "aes-key"],
            )],
        );

        let source = PatternKeySource::from_config(&config).unwrap();

        let eid = parse_eid("ipn:0.42.0");
        assert_eq!(
            source
                .key(&eid, &[Operation::Verify])
                .unwrap()
                .id
                .as_deref(),
            Some("hmac-key")
        );
        assert_eq!(
            source
                .key(&eid, &[Operation::Decrypt])
                .unwrap()
                .id
                .as_deref(),
            Some("aes-key")
        );
    }

    #[test]
    fn missing_key_file() {
        let config = Config {
            keys_file: PathBuf::from("/nonexistent/keys.jwks"),
            watch: WatchConfig::None,
            bindings: vec![],
        };
        assert!(PatternKeySource::from_config(&config).is_err());
    }

    #[test]
    fn unknown_kid_reference() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(&dir, VALID_KEYS);
        let config = config_with(
            &keys_path,
            vec![binding("ipn:*.*", SecurityRole::Verifier, &["nonexistent"])],
        );
        let err = PatternKeySource::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn duplicate_kid() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(
            &dir,
            r#"{ "keys": [
                { "kid": "dup", "kty": "oct", "k": "AAAA", "key_ops": ["verify"] },
                { "kid": "dup", "kty": "oct", "k": "BBBB", "key_ops": ["verify"] }
            ] }"#,
        );
        let config = config_with(
            &keys_path,
            vec![binding("ipn:*.*", SecurityRole::Verifier, &["dup"])],
        );
        let err = PatternKeySource::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn empty_binding_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(&dir, VALID_KEYS);
        let config = config_with(
            &keys_path,
            vec![binding("ipn:*.*", SecurityRole::Acceptor, &[])],
        );
        let err = PatternKeySource::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("no keys"));
    }

    #[test]
    fn key_without_ops_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(
            &dir,
            r#"{ "keys": [{ "kid": "no-ops", "kty": "oct", "k": "AAAA" }] }"#,
        );
        let config = config_with(
            &keys_path,
            vec![binding("ipn:*.*", SecurityRole::Verifier, &["no-ops"])],
        );
        let err = PatternKeySource::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("key_ops"));
    }

    #[test]
    fn non_symmetric_key_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(
            &dir,
            r#"{ "keys": [{ "kid": "ec", "kty": "EC", "key_ops": ["verify"] }] }"#,
        );
        let config = config_with(
            &keys_path,
            vec![binding("ipn:*.*", SecurityRole::Verifier, &["ec"])],
        );
        let err = PatternKeySource::from_config(&config).unwrap_err();
        assert!(err.to_string().contains("symmetric"));
    }

    #[test]
    fn no_bindings_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(&dir, VALID_KEYS);
        let config = config_with(&keys_path, vec![]);
        assert!(PatternKeySource::from_config(&config).is_ok());
    }
}
