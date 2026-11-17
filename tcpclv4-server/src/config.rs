use std::path::PathBuf;

use hardy_tcpclv4::config::Config as ClaConfig;
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

// Returns the default config directory, platform-specific:
// - Linux: /etc/hardy/
// - macOS: /etc/hardy/
// - Windows: %LOCALAPPDATA%\hardy\ (via `directories` crate), or exe directory as fallback
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

// Configuration for the standalone TCPCLv4 CLA server.
//
// Loaded from a TOML/YAML/JSON config file and/or environment variables
// prefixed with `HARDY_TCPCLV4_`. Uses kebab-case field names in config files
// and `__` as the nested-field separator for environment variables.
//
// The server's own keys sit at the top level, and the TCPCLv4 settings in a
// `cla` table beneath it (`HARDY_TCPCLV4_CLA__…` in the environment). Both
// levels refuse unknown keys, each naming the keys it knows.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
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

    // The TCPCLv4 settings: the shared schema of `hardy_tcpclv4::config`,
    // also read by `hardy-bpa-server`'s `clas:` entries.
    //
    // Default: every key absent, so every builder default applies.
    #[serde(default)]
    pub cla: ClaConfig,
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
        const CONFIG_FILE_VAR: &str = "HARDY_TCPCLV4_CONFIG_FILE";

        let config_file = config_file
            .or_else(|| std::env::var(CONFIG_FILE_VAR).ok().map(PathBuf::from))
            .unwrap_or_else(default_config_path);

        let source_file = config::File::with_name(&config_file.to_string_lossy());
        // `CONFIG_FILE_VAR` is consumed above to locate the file; exclude
        // it from the override source so the strict schema does not refuse
        // it as an unknown `config-file` key. Iterating the OS environment
        // skips non-Unicode variables instead of panicking on them.
        let overrides: config::Map<String, String> = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(key, _)| key != CONFIG_FILE_VAR)
            .collect();
        let source_env = config::Environment::with_prefix("HARDY_TCPCLV4")
            .prefix_separator("_")
            .separator("__")
            .convert_case(config::Case::Kebab)
            .try_parsing(true)
            .source(Some(overrides));

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
    use core::num::NonZeroU64;
    use std::io::Write;

    use hardy_async::TaskPool;
    use hardy_tcpclv4::{
        Error as Tcpclv4Error, KeepaliveInterval,
        tls::{ClientAuth, Error as TlsError},
    };
    use serial_test::serial;

    use super::*;
    use crate::server::Tcpclv4Server;

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
            config.cla.listeners.is_none(),
            "absent maps to the registered default listener"
        );
        assert!(
            config.cla.segment_mru.is_none(),
            "absent keys defer to the builder defaults"
        );
        assert!(config.cla.tls.is_none(), "plaintext by default");
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

[cla]
listeners = ["0.0.0.0:9999"]
segment-mru = 8192
keepalive-interval = 30
"#,
        );
        assert_eq!(config.bpa_address, "http://10.0.0.1:50051");
        assert_eq!(config.cla_name, "test-cla");
        assert_eq!(config.log_level, Level::DEBUG);
        assert_eq!(
            config.cla.listeners.unwrap(),
            vec![std::net::SocketAddr::from(([0, 0, 0, 0], 9999))]
        );
        assert_eq!(config.cla.segment_mru, Some(NonZeroU64::new(8192).unwrap()));
        assert_eq!(
            config.cla.keepalive_interval,
            Some(KeepaliveInterval::new(30))
        );
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
cla:
  segment-mru: 4096
"#,
        );
        assert_eq!(config.bpa_address, "http://10.0.0.2:50051");
        assert_eq!(config.cla_name, "yaml-cla");
        assert_eq!(config.log_level, Level::WARN);
        assert_eq!(config.cla.segment_mru, Some(NonZeroU64::new(4096).unwrap()));
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

    // Env vars override the TCPCLv4 fields through the `cla` table:
    // `HARDY_TCPCLV4_CLA__<KEY>`.
    #[test]
    #[serial]
    fn env_overrides_nested_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "").unwrap();

        unsafe { std::env::set_var("HARDY_TCPCLV4_CLA__SEGMENT_MRU", "32768") };
        let config = Config::load(Some(path)).unwrap();
        unsafe { std::env::remove_var("HARDY_TCPCLV4_CLA__SEGMENT_MRU") };

        assert_eq!(
            config.cla.segment_mru,
            Some(NonZeroU64::new(32768).unwrap())
        );
    }

    // `HARDY_TCPCLV4_CONFIG_FILE` is the loader's own interface, consumed
    // to locate the file; the strict schema must not refuse it as an
    // unknown `config-file` key.
    #[test]
    #[serial]
    fn config_file_env_var_is_not_a_schema_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("via-env.toml");
        std::fs::write(&path, "log-level = \"warn\"\n").unwrap();

        unsafe { std::env::set_var("HARDY_TCPCLV4_CONFIG_FILE", &path) };
        let result = Config::load(None);
        unsafe { std::env::remove_var("HARDY_TCPCLV4_CONFIG_FILE") };

        let config = result.expect("the loader's own env var must not be a schema error");
        assert_eq!(config.log_level, Level::WARN);
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
        std::fs::write(&path, "[cla]\nsegment-mru = -1").unwrap();
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
            std::fs::write(&path, format!("[cla]\n{content}")).unwrap();
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
        std::fs::write(&path, "[cla]\nlisteners = [\"not-an-address\"]").unwrap();
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
cla:
  tls:
    required: true
    ca-certs: "/etc/hardy/ca"
    identity:
      cert-file: "/etc/hardy/certs/server.crt"
      key-file: "/etc/hardy/private/server.key"
    client-auth: "required"
"#,
        );
        let tls = config.cla.tls.unwrap();
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
cla:
  tls:
    insecure-skip-verify: true
    identity:
      cert-file: "/etc/hardy/certs/server.crt"
      private-key-file: "/etc/hardy/private/server.key"
"#,
        );
        assert_eq!(
            config.cla.tls.unwrap().identity.unwrap().key_file,
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
            "[cla.tls]\ninsecure-skip-verify = true\n\n[cla.tls.identity]\ncert-file = \"c.pem\"\n",
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

    // Unknown keys are refused at each level with that level's known keys
    // listed, so a removed key or a typo cannot silently leave a default in
    // force.
    #[test]
    #[serial]
    fn unknown_fields_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("extra.toml");
        std::fs::write(&path, "log-level = \"warn\"\nthis-does-not-exist = 42\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(
            err.contains("this-does-not-exist") && err.contains("bpa-address"),
            "{err}"
        );

        // The removed single-listener key is refused with its replacement
        // among the known keys listed, the hint an operator migrating an
        // old file needs.
        let path = dir.path().join("address.toml");
        std::fs::write(&path, "[cla]\naddress = \"127.0.0.1:4556\"\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(
            err.contains("address") && err.contains("listeners"),
            "{err}"
        );

        // The quiet spellings of earlier schemas buy nothing: `tls.insecure`
        // is refused with the deliberately loud key named in the known list,
        // not silently ignored.
        let path = dir.path().join("bare-insecure.toml");
        std::fs::write(&path, "[cla.tls]\ninsecure = true\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("expected a parse error");
        };
        let err = err.to_string();
        assert!(err.contains("insecure-skip-verify"), "{err}");
    }

    // A TCPCLv4 key at the top level, the flat layout of earlier releases,
    // is refused with `cla` among the known keys, pointing at its new home;
    // the same holds for the flat environment spelling.
    #[test]
    #[serial]
    fn a_flat_tcpclv4_key_points_at_cla() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flat.toml");
        std::fs::write(&path, "segment-mru = 8192\n").unwrap();
        let Err(err) = Config::load(Some(path)) else {
            panic!("a top-level TCPCLv4 key must be refused");
        };
        let err = err.to_string();
        assert!(err.contains("segment-mru") && err.contains("cla"), "{err}");

        let path = dir.path().join("flat-env.toml");
        std::fs::write(&path, "").unwrap();
        unsafe { std::env::set_var("HARDY_TCPCLV4_SEGMENT_MRU", "8192") };
        let result = Config::load(Some(path));
        unsafe { std::env::remove_var("HARDY_TCPCLV4_SEGMENT_MRU") };
        let Err(err) = result else {
            panic!("a flat TCPCLv4 env var must be refused");
        };
        let err = err.to_string();
        assert!(err.contains("segment-mru") && err.contains("cla"), "{err}");
    }

    // The `config` crate coerces a TCPCLv4 key's spelling to its type, in a
    // file and in the environment: `yes` and `1` for a bool, a quoted
    // number for an integer.
    #[test]
    #[serial]
    fn tcpclv4_scalars_are_coerced() {
        let config = write_and_load(
            "coerce.yaml",
            "cla:\n  segment-mru: \"8192\"\n  tls:\n    insecure-skip-verify: true\n    required: yes\n",
        );
        assert_eq!(config.cla.segment_mru, NonZeroU64::new(8192));
        assert!(config.cla.tls.unwrap().required);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coerce-env.toml");
        std::fs::write(&path, "[cla.tls]\ninsecure-skip-verify = true\n").unwrap();
        unsafe { std::env::set_var("HARDY_TCPCLV4_CLA__TLS__REQUIRED", "1") };
        let result = Config::load(Some(path));
        unsafe { std::env::remove_var("HARDY_TCPCLV4_CLA__TLS__REQUIRED") };
        assert!(result.unwrap().cla.tls.unwrap().required);
    }

    // The shipped example files parse against the schema, so they cannot
    // drift from it unnoticed.
    #[test]
    #[serial]
    fn example_configs_parse() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for name in ["example_config.toml", "example_config.yaml"] {
            Config::load(Some(crate_dir.join(name)))
                .unwrap_or_else(|e| panic!("{name} must parse: {e}"));
        }
    }

    // Large segment-mru value is accepted.
    #[test]
    #[serial]
    fn large_segment_mru() {
        let config = write_and_load("large.toml", "[cla]\nsegment-mru = 1073741824\n");
        assert_eq!(
            config.cla.segment_mru,
            Some(NonZeroU64::new(1073741824).unwrap())
        );
    }

    // The dial-only spelling is an empty listener list: nothing is bound.
    #[test]
    #[serial]
    fn empty_listeners_is_dial_only() {
        let config = write_and_load("dial_only.yaml", "cla:\n  listeners: []\n");
        assert_eq!(config.cla.listeners, Some(vec![]));
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
        std::fs::write(&path, "cla:\n  keepalive-interval: null\n").unwrap();
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
            "[cla]\nlisteners = [\"127.0.0.1:0\"]\nkeepalive-interval = 0\n",
        );
        assert_eq!(
            config.cla.keepalive_interval,
            Some(KeepaliveInterval::DISABLED)
        );
        Tcpclv4Server::new(config, TaskPool::new()).expect("0 disables keepalives");
    }

    // Building the server delegates to the library's conversion, whose
    // rules `hardy-tcpclv4`'s own tests cover: a contradiction surfaces as
    // the library's typed error.
    #[test]
    #[serial]
    fn building_delegates_to_the_library() {
        let config = write_and_load(
            "no-anchor.toml",
            "[cla]\nlisteners = []\n\n[cla.tls]\nrequired = true\n",
        );
        let Err(err) = Tcpclv4Server::new(config, TaskPool::new()) else {
            panic!("TLS without a trust anchor must fail to build");
        };
        assert!(
            matches!(
                err.downcast_ref::<Tcpclv4Error>(),
                Some(Tcpclv4Error::Tls(TlsError::NoTrustAnchor))
            ),
            "{err:?}"
        );
    }
}
