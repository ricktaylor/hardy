use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hardy_bpa::key::pattern::PatternKeySource;
use hardy_bpv7::bpsec::key::{KeySet, Type};
use hardy_eid_patterns::EidPattern;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::watcher::WatchMode;

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    /// Path to a JWK Set file (RFC 7517 Section 5).
    /// The file SHOULD have restrictive permissions (0600 on Unix).
    pub keys_file: PathBuf,

    /// Watch the key file for changes and reload automatically.
    /// Values: "native" (inotify/kqueue), "poll" (works in Docker). Absent to disable.
    #[serde(default)]
    pub watch: Option<WatchMode>,

    /// Key bindings: map EID patterns to keys by kid.
    /// Evaluated by specificity (most specific match wins).
    #[serde(default)]
    pub bindings: Vec<KeyBindingConfig>,
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct KeyBindingConfig {
    /// EID pattern to match against the security source EID.
    #[serde(rename = "match")]
    pub pattern: EidPattern,

    /// kid of the key for integrity operations (BIB sign/verify).
    pub integrity_key: Option<String>,

    /// kid of the key for confidentiality operations (BCB encrypt/decrypt).
    pub confidentiality_key: Option<String>,
}

impl Config {
    /// Load keys and build a `PatternKeySource` from this config.
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
                warn!("Key file contains a key without a 'kid' field — skipping");
                continue;
            };

            if !matches!(key.key_type, Type::OctetSequence { ref key } if !key.is_empty()) {
                anyhow::bail!("Key '{kid}' must be a non-empty symmetric key (kty: oct)");
            }

            if key.operations.is_none() {
                anyhow::bail!("Key '{kid}' has no 'key_ops' field — cannot match any operation");
            }

            if keys.insert(kid.clone(), key).is_some() {
                anyhow::bail!("Key file contains duplicate kid '{kid}'");
            }
        }

        // Validate all kid references and binding completeness
        for binding in &self.bindings {
            if binding.integrity_key.is_none() && binding.confidentiality_key.is_none() {
                anyhow::bail!(
                    "Security binding for '{}' has neither integrity-key nor confidentiality-key",
                    binding.pattern
                );
            }

            for kid in binding
                .integrity_key
                .iter()
                .chain(binding.confidentiality_key.iter())
            {
                if !keys.contains_key(kid) {
                    anyhow::bail!("Security binding references unknown key id '{kid}'");
                }
            }
        }

        let bindings = self
            .bindings
            .iter()
            .map(|b| {
                (
                    b.pattern.clone(),
                    b.integrity_key.clone(),
                    b.confidentiality_key.clone(),
                )
            })
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
    use super::*;

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
            watch: None,
            bindings,
        }
    }

    fn binding(
        pattern: &str,
        integrity: Option<&str>,
        confidentiality: Option<&str>,
    ) -> KeyBindingConfig {
        KeyBindingConfig {
            pattern: pattern.parse().unwrap(),
            integrity_key: integrity.map(String::from),
            confidentiality_key: confidentiality.map(String::from),
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
            vec![binding("ipn:*.*", Some("hmac-key"), Some("aes-key"))],
        );

        let source = config.build().unwrap();

        use hardy_bpv7::bpsec::key::{KeySource, Operation};
        use hardy_bpv7::eid::Eid;

        let eid: Eid = "ipn:0.42.0".parse().unwrap();
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
            watch: None,
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
            vec![binding("ipn:*.*", Some("nonexistent"), None)],
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
        let config = config_with(&keys_path, vec![binding("ipn:*.*", Some("dup"), None)]);
        let err = config.build().unwrap_err();
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn empty_binding_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(&dir, VALID_KEYS);
        let config = config_with(&keys_path, vec![binding("ipn:*.*", None, None)]);
        let err = config.build().unwrap_err();
        assert!(err.to_string().contains("neither"));
    }

    #[test]
    fn key_without_ops_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let keys_path = write_keys(
            &dir,
            r#"{ "keys": [{ "kid": "no-ops", "kty": "oct", "k": "AAAA" }] }"#,
        );
        let config = config_with(&keys_path, vec![binding("ipn:*.*", Some("no-ops"), None)]);
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
        let config = config_with(&keys_path, vec![binding("ipn:*.*", Some("ec"), None)]);
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
