use super::WatchConfig;
use crate::bpsec::SecurityRole;
use hardy_eid_patterns::EidPattern;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
