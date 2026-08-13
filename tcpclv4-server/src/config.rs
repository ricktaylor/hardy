use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::net::SocketAddr;
use std::path::PathBuf;

use hardy_tcpclv4::{ContactTimeout, KeepaliveInterval, tls};
use serde::{Deserialize, Serialize};
use tracing::Level;

mod log_level_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::str::FromStr;
    use tracing::Level;

    pub fn serialize<S>(level: &Level, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(level.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Level, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Level::from_str(&s).map_err(serde::de::Error::custom)
    }
}

// The library's `ContactTimeout` carries the RFC 9174 Section 4.2 range as
// its invariant but no serde impls (the library does not parse
// configuration), so this adapter is where an out-of-range value is
// rejected at parse.
mod contact_timeout_serde {
    use hardy_tcpclv4::ContactTimeout;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(timeout: &Option<ContactTimeout>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match timeout {
            Some(timeout) => serializer.serialize_some(&timeout.get()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<ContactTimeout>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<u16>::deserialize(deserializer)?
            .map(|seconds| {
                ContactTimeout::new(seconds).ok_or_else(|| {
                    serde::de::Error::custom(
                        "contact-timeout must be between 1 and 60 seconds (RFC 9174 Section 4.2)",
                    )
                })
            })
            .transpose()
    }
}

// `KeepaliveInterval` carries the wire's zero-is-disabled encoding (every
// u16 is a valid value) but has no serde impls (the library does not
// parse configuration), so this adapter is the conversion.
mod keepalive_interval_serde {
    use hardy_tcpclv4::KeepaliveInterval;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(
        interval: &Option<KeepaliveInterval>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match interval {
            Some(interval) => serializer.serialize_some(&interval.get()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<KeepaliveInterval>, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Option::<u16>::deserialize(deserializer)? {
            // An explicit `null` was an earlier schema's disabled spelling;
            // refuse it rather than silently re-enable keepalives.
            None => Err(serde::de::Error::custom(
                "keepalive-interval does not accept null: use 0 to disable keepalives, or omit the key for the default",
            )),
            Some(seconds) => Ok(Some(KeepaliveInterval::new(seconds))),
        }
    }
}

// Returns the default config directory, platform-specific:
// - Linux: /etc/hardy/
// - macOS: /etc/hardy/
// - Windows: %ProgramData%\hardy\ (via `directories` crate), or exe directory as fallback
fn default_config_dir() -> PathBuf {
    #[cfg(unix)]
    return PathBuf::from("/etc/hardy");

    #[cfg(windows)]
    return directories::BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("hardy"))
        .unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        });
}

fn default_config_path() -> PathBuf {
    default_config_dir().join("tcpclv4")
}

fn default_log_level() -> Level {
    Level::INFO
}

fn default_bpa_address() -> String {
    "http://[::1]:50051".to_string()
}

fn default_cla_name() -> String {
    env!("CARGO_PKG_NAME").to_string()
}

// A certificate and the private key that proves it: only representable as
// a pair.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Identity {
    // The node's certificate (PEM).
    pub cert_file: PathBuf,

    // The private key (PEM: PKCS#8, PKCS#1, or SEC1) matching `cert-file`.
    #[serde(alias = "private-key-file")]
    pub key_file: PathBuf,
}

// Client-certificate verification policy for inbound TLS connections
// (mutual TLS): `required` refuses dialers without a certificate chaining
// to `ca-certs`; `optional` verifies a certificate when one is presented
// but accepts dialers without one; `off` never requests one.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClientAuth {
    #[default]
    Off,
    Optional,
    Required,
}

// The config schema's client-auth policy is a mirror of the library's;
// this conversion is the one place the two are stitched together.
impl From<ClientAuth> for tls::ClientAuth {
    fn from(policy: ClientAuth) -> Self {
        match policy {
            ClientAuth::Off => Self::Off,
            ClientAuth::Optional => Self::Optional,
            ClientAuth::Required => Self::Required,
        }
    }
}

// The `tls` section of the config file. The serde layer stays permissive
// and flat, mirroring what the operator types; `identity` is one object
// with two required fields, so a lone certificate or key cannot be
// written, and `required` lives inside the section, so "require TLS
// without configuring TLS" cannot be written. The trust-anchor rules are
// judged by `Tcpclv4Server::new`, which names the keys in its errors; the
// honest make-invalid-unrepresentable types live behind it, in
// `hardy_tcpclv4::tls`.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct TlsConfig {
    // Refuse sessions that do not negotiate TLS. Default: `false`.
    #[serde(default)]
    pub required: bool,

    // Directory of PEM CA certificates used to verify peers'
    // certificates: the standing trust anchor for normal operation.
    #[serde(default)]
    pub ca_certs: Option<PathBuf>,

    // Accept any peer certificate chain with no trust validation
    // (INSECURE; testing only). The key is deliberately loud and has no
    // shorter alias: the danger must be visible in the file. Overrides
    // `ca-certs` when both are set, so a debug session is one line to
    // flip; the override is named in a startup warning, and the ignored
    // bundle is never loaded.
    #[serde(default)]
    pub insecure_skip_verify: bool,

    // The node's own certificate and private key. Required to accept TLS
    // connections (the listener's TLS server role), and presented to
    // dialed peers under mutual TLS.
    #[serde(default)]
    pub identity: Option<Identity>,

    // Client-certificate verification for inbound connections (mutual
    // TLS). Requires `identity` and a `ca-certs` trust anchor.
    #[serde(default)]
    pub client_auth: ClientAuth,

    // SNI override presented when dialing (for certificates issued to
    // domain names).
    #[serde(default)]
    pub server_name: Option<String>,
}

// Configuration for the standalone TCPCLv4 CLA server.
//
// Loaded from a TOML/YAML/JSON config file and/or environment variables
// prefixed with `HARDY_TCPCLV4_`. Uses kebab-case field names in config files
// and `__` as the nested-field separator for environment variables.
//
// The transport fields mirror the `hardy_tcpclv4` builder inputs (kept in
// sync with the `clas:` entry mirror in bpa-server/src/config/tcpclv4.rs).
// Absent keys stay `None` and leave the corresponding builder default in
// force, so no default value is restated here. Scalar invariants are
// carried by the schema types (`NonZero` integers, the `contact-timeout`
// adapter) and rejected at parse; `Tcpclv4Server::new` (src/server.rs)
// maps the file surface onto the builder, naming config keys in the
// errors that remain.
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    // Logging level for the tracing subscriber.
    //
    // Default: `INFO`.
    #[serde(default = "default_log_level", with = "log_level_serde")]
    pub log_level: Level,

    // The gRPC endpoint of the BPA to register with.
    //
    // Default: `"http://[::1]:50051"`.
    #[serde(default = "default_bpa_address")]
    pub bpa_address: String,

    // The name used to identify this CLA instance when registering with the BPA.
    //
    // Default: the crate package name (`"tcpclv4-server"`).
    #[serde(default = "default_cla_name")]
    pub cla_name: String,

    // Peer addresses to connect to on startup (e.g. ["bpa2:4556"]).
    // Each entry is resolved via DNS and a TCPCLv4 session is established.
    #[serde(default)]
    pub peers: Vec<String>,

    // The passive listening elements, one address per listener; absent
    // listens on the IANA-registered `[::]:4556`, and an empty list
    // disables listening (dial-only).
    #[serde(default)]
    pub listeners: Option<Vec<SocketAddr>>,

    // Largest acceptable single-segment payload, in bytes; zero is
    // rejected at parse.
    #[serde(default)]
    pub segment_mru: Option<NonZeroU64>,

    // Largest acceptable total bundle transfer, in bytes; zero is
    // rejected at parse.
    #[serde(default)]
    pub transfer_mru: Option<NonZeroU64>,

    // Idle connections retained per remote address; 0 disables pooling.
    #[serde(default)]
    pub max_idle_connections: Option<usize>,

    // Transfers accepted but not yet resolved with an outcome, per peer;
    // zero is rejected at parse. Bounds the bundles held in memory by
    // in-flight and queued transfers to each peer.
    #[serde(default)]
    pub max_outstanding_transfers: Option<NonZeroUsize>,

    // Inbound connections accepted per second; zero is rejected at parse.
    #[serde(default)]
    pub connection_rate_limit: Option<NonZeroU32>,

    // Seconds to wait for a peer's contact header: 1 to 60 (RFC 9174
    // Section 4.2); out-of-range values are rejected at parse.
    #[serde(default, with = "contact_timeout_serde")]
    pub contact_timeout: Option<ContactTimeout>,

    // Keepalive interval in seconds (RFC 9174 Section 4.7); 0 disables
    // keepalives.
    #[serde(default, with = "keepalive_interval_serde")]
    pub keepalive_interval: Option<KeepaliveInterval>,

    // TLS configuration; absent means plaintext.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

impl Config {
    // Load configuration from a file and environment variable overrides.
    //
    // Resolution order for the config file path:
    // 1. The explicit `config_file` argument (if `Some`).
    // 2. The `HARDY_TCPCLV4_CONFIG_FILE` environment variable (if set).
    // 3. The platform-specific default path (e.g. `/etc/hardy/tcpclv4` on Linux).
    //
    // Environment variables prefixed with `HARDY_TCPCLV4_` override values
    // from the config file.
    pub fn load(config_file: Option<PathBuf>) -> anyhow::Result<Config> {
        let config_file = config_file
            .or_else(|| {
                std::env::var("HARDY_TCPCLV4_CONFIG_FILE")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or_else(default_config_path);

        let source_file = config::File::with_name(&config_file.to_string_lossy());
        let source_env = config::Environment::with_prefix("HARDY_TCPCLV4")
            .prefix_separator("_")
            .separator("__")
            .convert_case(config::Case::Kebab)
            .try_parsing(true);

        let config = config::Config::builder()
            .add_source(source_file)
            .add_source(source_env)
            .build()?
            .try_deserialize()?;

        eprintln!("Loaded configuration from '{}'", config_file.display());
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Tcpclv4Server;
    use hardy_async::TaskPool;
    use serial_test::serial;
    use std::io::Write;

    // Helper: write a config file and load it.
    fn write_and_load(name: &str, content: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        Config::load(Some(path)).unwrap()
    }

    // Empty config file produces sensible defaults.
    #[test]
    #[serial]
    fn empty_config_has_defaults() {
        let config = write_and_load("empty.toml", "");
        assert_eq!(config.bpa_address, "http://[::1]:50051");
        assert_eq!(config.cla_name, env!("CARGO_PKG_NAME"));
        assert_eq!(config.log_level, Level::INFO);
        assert!(
            config.listeners.is_none(),
            "absent maps to the registered default listener"
        );
        assert!(
            config.segment_mru.is_none(),
            "absent keys defer to the builder defaults"
        );
        assert!(config.tls.is_none(), "plaintext by default");
        // Not built here: the all-defaults build would bind the real
        // IANA-registered [::]:4556. The build path is covered by the
        // tests below, which listen on an ephemeral loopback port.
    }

    // TOML config file overrides defaults.
    #[test]
    #[serial]
    fn toml_overrides_defaults() {
        let config = write_and_load(
            "test.toml",
            r#"
bpa-address = "http://10.0.0.1:50051"
cla-name = "test-cla"
log-level = "debug"
listeners = ["0.0.0.0:9999"]
segment-mru = 8192
keepalive-interval = 30
"#,
        );
        assert_eq!(config.bpa_address, "http://10.0.0.1:50051");
        assert_eq!(config.cla_name, "test-cla");
        assert_eq!(config.log_level, Level::DEBUG);
        assert_eq!(
            config.listeners.unwrap(),
            vec![std::net::SocketAddr::from(([0, 0, 0, 0], 9999))]
        );
        assert_eq!(config.segment_mru, Some(NonZeroU64::new(8192).unwrap()));
        assert_eq!(config.keepalive_interval, Some(KeepaliveInterval::new(30)));
    }

    // YAML config file works identically to TOML.
    #[test]
    #[serial]
    fn yaml_config() {
        let config = write_and_load(
            "test.yaml",
            r#"
bpa-address: "http://10.0.0.2:50051"
cla-name: "yaml-cla"
log-level: "warn"
segment-mru: 4096
"#,
        );
        assert_eq!(config.bpa_address, "http://10.0.0.2:50051");
        assert_eq!(config.cla_name, "yaml-cla");
        assert_eq!(config.log_level, Level::WARN);
        assert_eq!(config.segment_mru, Some(NonZeroU64::new(4096).unwrap()));
    }

    // JSON config file works identically to TOML.
    #[test]
    #[serial]
    fn json_config() {
        let config = write_and_load(
            "test.json",
            r#"{
    "bpa-address": "http://10.0.0.3:50051",
    "cla-name": "json-cla",
    "log-level": "error"
}"#,
        );
        assert_eq!(config.bpa_address, "http://10.0.0.3:50051");
        assert_eq!(config.cla_name, "json-cla");
        assert_eq!(config.log_level, Level::ERROR);
    }

    // Environment variables override config file values.
    #[test]
    #[serial]
    fn env_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"
bpa-address = "http://file-value:50051"
cla-name = "file-cla"
log-level = "warn"
"#
        )
        .unwrap();

        unsafe { std::env::set_var("HARDY_TCPCLV4_BPA_ADDRESS", "http://env-value:50051") };
        unsafe { std::env::set_var("HARDY_TCPCLV4_LOG_LEVEL", "error") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_TCPCLV4_BPA_ADDRESS") };
        unsafe { std::env::remove_var("HARDY_TCPCLV4_LOG_LEVEL") };

        assert_eq!(
            config.bpa_address, "http://env-value:50051",
            "env var should override file"
        );
        assert_eq!(
            config.cla_name, "file-cla",
            "non-overridden value should come from file"
        );
        assert_eq!(
            config.log_level,
            Level::ERROR,
            "env var should override log level"
        );
    }

    // Env vars override the top-level transport fields.
    #[test]
    #[serial]
    fn env_overrides_nested_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "").unwrap();

        unsafe { std::env::set_var("HARDY_TCPCLV4_SEGMENT_MRU", "32768") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_TCPCLV4_SEGMENT_MRU") };

        assert_eq!(config.segment_mru, Some(NonZeroU64::new(32768).unwrap()));
    }

    // Missing config file returns an error.
    #[test]
    #[serial]
    fn missing_config_file_errors() {
        let result = Config::load(Some(PathBuf::from("/nonexistent/path/config")));
        assert!(result.is_err());
    }

    // Invalid log level in config file returns an error.
    #[test]
    #[serial]
    fn invalid_log_level_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "log-level = \"banana\"").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Negative segment-mru is rejected.
    #[test]
    #[serial]
    fn negative_segment_mru_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "segment-mru = -1").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Zero and out-of-range scalars are rejected when the file is parsed,
    // before any mapping onto the builder runs: the schema carries the
    // invariants as `NonZero` types and the `ContactTimeout` adapter.
    #[test]
    #[serial]
    fn parse_rejects_invalid_scalars() {
        for (name, content, message) in [
            ("zero-segment-mru.toml", "segment-mru = 0\n", "nonzero"),
            ("zero-transfer-mru.toml", "transfer-mru = 0\n", "nonzero"),
            (
                "zero-rate-limit.toml",
                "connection-rate-limit = 0\n",
                "nonzero",
            ),
            (
                "zero-outstanding-transfers.toml",
                "max-outstanding-transfers = 0\n",
                "nonzero",
            ),
            (
                "contact-timeout-zero.toml",
                "contact-timeout = 0\n",
                "between 1 and 60",
            ),
            (
                "contact-timeout-oob.toml",
                "contact-timeout = 61\n",
                "between 1 and 60",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(name);
            std::fs::write(&path, content).unwrap();
            let Err(err) = Config::load(Some(path)) else {
                panic!("{name}: expected a parse error");
            };
            let err = err.to_string();
            assert!(err.contains(message), "{name}: {err}");
        }
    }

    // An invalid listener address is rejected.
    #[test]
    #[serial]
    fn invalid_listener_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "listeners = [\"not-an-address\"]").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // A full TLS section parses into its structured form.
    #[test]
    #[serial]
    fn tls_full_config() {
        let config = write_and_load(
            "tls.yaml",
            r#"
tls:
  required: true
  ca-certs: "/etc/hardy/ca"
  identity:
    cert-file: "/etc/hardy/certs/server.crt"
    key-file: "/etc/hardy/private/server.key"
  client-auth: "required"
"#,
        );
        let tls = config.tls.unwrap();
        assert!(tls.required);
        assert_eq!(tls.ca_certs, Some(PathBuf::from("/etc/hardy/ca")));
        assert!(!tls.insecure_skip_verify);
        let identity = tls.identity.unwrap();
        assert_eq!(
            identity.cert_file,
            PathBuf::from("/etc/hardy/certs/server.crt")
        );
        assert_eq!(
            identity.key_file,
            PathBuf::from("/etc/hardy/private/server.key")
        );
        assert_eq!(tls.client_auth, ClientAuth::Required);
    }

    // The pre-rename key `private-key-file` is still accepted as an alias
    // for `key-file`.
    #[test]
    #[serial]
    fn private_key_file_alias() {
        let config = write_and_load(
            "alias.yaml",
            r#"
tls:
  insecure-skip-verify: true
  identity:
    cert-file: "/etc/hardy/certs/server.crt"
    private-key-file: "/etc/hardy/private/server.key"
"#,
        );
        assert_eq!(
            config.tls.unwrap().identity.unwrap().key_file,
            PathBuf::from("/etc/hardy/private/server.key")
        );
    }

    // The identity's shape rejects a lone certificate or key at parse
    // time: both fields of `tls.identity` are required, so half a pair
    // never reaches the builder mapping.
    #[test]
    #[serial]
    fn lone_identity_half_rejected_at_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lone-cert-half.toml");
        std::fs::write(
            &path,
            "[tls]\ninsecure-skip-verify = true\n\n[tls.identity]\ncert-file = \"c.pem\"\n",
        )
        .unwrap();
        assert!(
            Config::load(Some(path)).is_err(),
            "expected a parse error for an identity missing its key-file"
        );
    }

    // Malformed TOML returns an error.
    #[test]
    #[serial]
    fn malformed_toml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "bpa-address = \n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Malformed YAML returns an error.
    #[test]
    #[serial]
    fn malformed_yaml_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "bpa-address: [broken\n").unwrap();
        let result = Config::load(Some(path));
        assert!(result.is_err());
    }

    // Unknown fields are silently ignored.
    #[test]
    #[serial]
    fn unknown_fields_ignored() {
        let config = write_and_load(
            "extra.toml",
            r#"
log-level = "warn"
this-does-not-exist = 42
"#,
        );
        assert_eq!(config.log_level, Level::WARN);
    }

    // Large segment-mru value is accepted.
    #[test]
    #[serial]
    fn large_segment_mru() {
        let config = write_and_load("large.toml", "segment-mru = 1073741824\n");
        assert_eq!(
            config.segment_mru,
            Some(NonZeroU64::new(1073741824).unwrap())
        );
    }

    // The dial-only spelling is an empty listener list: nothing is bound.
    #[test]
    #[serial]
    fn empty_listeners_is_dial_only() {
        let config = write_and_load("dial_only.yaml", "listeners: []\n");
        assert_eq!(config.listeners, Some(vec![]));
        Tcpclv4Server::new(config, TaskPool::new()).expect("a dial-only build binds nothing");
    }

    // `null` is not a spelling in this schema: an earlier schema used it to
    // disable keepalives, so it is refused with the replacement named
    // rather than silently mapped to the default.
    #[test]
    #[serial]
    fn keepalive_null_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("null-keepalive.yaml");
        std::fs::write(&path, "keepalive-interval: null\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("keepalive"), "{err}");
    }

    // Keepalive interval of 0 disables keepalives, and still builds.
    #[test]
    #[serial]
    fn keepalive_zero() {
        let config = write_and_load(
            "keepalive.toml",
            "listeners = [\"127.0.0.1:0\"]\nkeepalive-interval = 0\n",
        );
        assert_eq!(config.keepalive_interval, Some(KeepaliveInterval::DISABLED));
        Tcpclv4Server::new(config, TaskPool::new()).expect("0 disables keepalives");
    }

    // The mapping onto the builder rejects each config contradiction with
    // a message naming the offending keys, before any file is touched.
    #[test]
    #[serial]
    fn mapping_rejects_contradictions() {
        for (name, content, message) in [
            ("no-anchor.toml", "[tls]\nrequired = true\n", "trust anchor"),
            (
                "insecure-off-is-no-anchor.toml",
                "[tls]\ninsecure-skip-verify = false\n",
                "trust anchor",
            ),
            // The quiet spellings of earlier schemas buy nothing: an
            // unknown key is ignored, leaving no trust anchor configured.
            (
                "bare-insecure.toml",
                "[tls]\ninsecure = true\n",
                "trust anchor",
            ),
            (
                "auth-without-identity.toml",
                "[tls]\nca-certs = \"/etc/hardy/ca\"\nclient-auth = \"required\"\n",
                "identity",
            ),
            (
                "auth-insecure.toml",
                "[tls]\ninsecure-skip-verify = true\nclient-auth = \"optional\"\n\n[tls.identity]\ncert-file = \"c.pem\"\nkey-file = \"k.pem\"\n",
                "insecure-skip-verify",
            ),
        ] {
            let Err(err) = Tcpclv4Server::new(write_and_load(name, content), TaskPool::new())
            else {
                panic!("{name}: expected a mapping error");
            };
            let err = err.to_string();
            assert!(err.contains(message), "{name}: {err}");
        }
    }

    // An insecure-only TLS config builds without touching any file.
    #[test]
    #[serial]
    fn insecure_only_builds() {
        let config = write_and_load(
            "insecure.toml",
            "listeners = [\"127.0.0.1:0\"]\n\n[tls]\ninsecure-skip-verify = true\n",
        );
        Tcpclv4Server::new(config, TaskPool::new()).expect("no file IO on this path");
    }

    // insecure-skip-verify overrides ca-certs rather than conflicting with
    // it, so a debug session is one line to flip. The ca-certs path is
    // bogus on purpose: the build succeeding proves the ignored bundle is
    // never loaded.
    #[test]
    #[serial]
    fn insecure_overrides_ca_certs() {
        let config = write_and_load(
            "override.toml",
            "listeners = [\"127.0.0.1:0\"]\n\n[tls]\nca-certs = \"/nonexistent/ca\"\ninsecure-skip-verify = true\n",
        );
        Tcpclv4Server::new(config, TaskPool::new())
            .expect("insecure-skip-verify wins; ca-certs is not loaded");
    }
}
