use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use hardy_bpv7::bpsec::key::{KeySet, Type};
use hardy_eid_patterns::EidPattern;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::WatchConfig;
use crate::bpsec::{PatternKeySource, SecurityRole};

/// BPSec configuration: the JWKS key file and its EID-pattern key bindings.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    /// Path to a JWK Set file (RFC 7517 Section 5).
    /// The file SHOULD have restrictive permissions (0600 on Unix).
    pub keys_file: PathBuf,

    /// Watch the key file for changes and reload automatically.
    /// Values: "native" (default), "poll" (works in Docker), "none" to disable.
    #[serde(default)]
    pub watch: WatchConfig,

    /// Key bindings: map EID patterns to keys and roles.
    /// Evaluated by specificity (most specific match wins).
    #[serde(default)]
    pub bindings: Vec<KeyBindingConfig>,
}

/// A single key binding: an EID pattern, a security role, and the bound key ids.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct KeyBindingConfig {
    /// EID pattern to match against the security source EID.
    #[serde(rename = "match")]
    pub pattern: EidPattern,

    /// Security role: "verifier" (default), "acceptor", or "source".
    /// The role gates which operations keys are released for: a verifier
    /// releases keys only to verify BIBs, so BCBs ride through encrypted;
    /// an acceptor additionally releases decrypt keys.
    #[serde(default)]
    pub role: SecurityRole,

    /// Key IDs (kids) bound to this pattern.
    /// The BPSec backend selects the right key by matching the
    /// required operation against each key's `key_ops` field.
    #[serde(default)]
    pub keys: Vec<String>,
}

impl Config {
    /// Loads the JWKS file named in this configuration and builds a
    /// [`PatternKeySource`].
    ///
    /// Every key must be a non-empty symmetric key (`kty: oct`) carrying a
    /// `key_ops` field, and every binding must reference a known key id.
    pub fn build(&self) -> anyhow::Result<PatternKeySource> {
        check_permissions(&self.keys_file);

        let file = std::fs::File::open(&self.keys_file).map_err(|e| {
            anyhow::anyhow!(
                "Failed to open key file '{}': {e}",
                self.keys_file.display()
            )
        })?;

        let key_set: KeySet = serde_json::from_reader(file).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse key file '{}': {e}",
                self.keys_file.display()
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
        for binding in &self.bindings {
            if binding.keys.is_empty() {
                anyhow::bail!("Security binding for '{}' has no keys", binding.pattern);
            }

            for kid in &binding.keys {
                if !keys.contains_key(kid) {
                    anyhow::bail!("Security binding references unknown key id '{kid}'");
                }
            }
        }

        let bindings = self
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

#[cfg(test)]
mod tests {
    use hardy_bpv7::{
        bpsec::key::{KeySource, Operation},
        eid::Eid,
    };

    use super::*;

    fn parse_eid(s: &str) -> Eid {
        s.parse().expect("valid EID")
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

        let source = config.build().unwrap();

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
        assert!(config.build().is_err());
    }

    #[test]
    fn unknown_kid_reference() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(&dir, VALID_KEYS);
        let config = config_with(
            &keys_path,
            vec![binding("ipn:*.*", SecurityRole::Verifier, &["nonexistent"])],
        );
        let err = config.build().unwrap_err();
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
        let err = config.build().unwrap_err();
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
        let err = config.build().unwrap_err();
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
        let err = config.build().unwrap_err();
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
        let err = config.build().unwrap_err();
        assert!(err.to_string().contains("symmetric"));
    }

    #[test]
    fn no_bindings_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(&dir, VALID_KEYS);
        let config = config_with(&keys_path, vec![]);
        assert!(config.build().is_ok());
    }
}
