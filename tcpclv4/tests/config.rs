//! `config::Config` converts into a factory. Each conversion test
//! proves one field reaches the builder, through an outcome the public API
//! exposes (the declared receive limit, or the typed error the builder
//! raises for that input). The schema module tests the serde rules the
//! library owns.

use core::num::NonZeroU64;
use std::net::TcpListener;

use hardy_bpa::cla::Factory as _;
use hardy_tcpclv4::{
    Error, Tcpclv4,
    builder::Tcpclv4Builder,
    config::{Config, Identity, TlsConfig},
    tls::{self, ClientAuth},
};
use tempfile::tempdir;

// Binds nothing, so the conversion touches no socket.
fn dial_only() -> Config {
    Config {
        listeners: Some(Vec::new()),
        ..Default::default()
    }
}

// The error a config the build must reject raises. The factory is
// opaque, so the rejection is taken as a let-else rather than
// `unwrap_err`.
fn rejected(config: Config) -> Error {
    let Err(err) = Tcpclv4::factory(config) else {
        panic!("the build must reject this configuration");
    };
    err
}

// Accepts any peer, so the conversion loads no CA certificates.
fn insecure_tls() -> TlsConfig {
    TlsConfig {
        insecure_skip_verify: true,
        ..Default::default()
    }
}

#[test]
fn transfer_mru_is_the_declared_receive_limit() {
    let mru = NonZeroU64::new(65536).unwrap();
    let factory = Tcpclv4::factory(Config {
        transfer_mru: Some(mru),
        ..dial_only()
    })
    .expect("a dial-only plaintext build cannot fail");
    assert_eq!(factory.request().max_reception_size, Some(mru));
}

#[test]
fn listeners_are_bound() {
    // The held socket makes the bind fail, naming the configured address.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = held.local_addr().unwrap();
    let err = rejected(Config {
        listeners: Some(vec![address]),
        ..Default::default()
    });
    assert!(
        matches!(err, Error::BindListener { address: bound, .. } if bound == address),
        "{err:?}"
    );
}

#[test]
fn tls_requires_a_trust_anchor() {
    let err = rejected(Config {
        tls: Some(TlsConfig::default()),
        ..dial_only()
    });
    assert!(
        matches!(err, Error::Tls(tls::Error::NoTrustAnchor)),
        "{err:?}"
    );
}

#[test]
fn insecure_skip_verify_is_a_trust_anchor() {
    Tcpclv4::factory(Config {
        tls: Some(insecure_tls()),
        ..dial_only()
    })
    .expect("insecure-skip-verify stands in for ca-certs");
}

#[test]
fn ca_certs_are_loaded() {
    let dir = tempdir().unwrap();
    let ca_certs = dir.path().join("missing-ca");
    let err = rejected(Config {
        tls: Some(TlsConfig {
            ca_certs: Some(ca_certs.clone()),
            ..Default::default()
        }),
        ..dial_only()
    });
    assert!(
        matches!(&err, Error::Tls(tls::Error::CaCertsMissing { path }) if *path == ca_certs),
        "{err:?}"
    );
}

#[test]
fn identity_is_loaded() {
    let dir = tempdir().unwrap();
    let cert_file = dir.path().join("missing-cert.pem");
    let err = rejected(Config {
        tls: Some(TlsConfig {
            identity: Some(Identity {
                cert_file: cert_file.clone(),
                key_file: dir.path().join("missing-key.pem"),
            }),
            ..insecure_tls()
        }),
        ..dial_only()
    });
    assert!(
        matches!(&err, Error::Tls(tls::Error::LoadCertificate { path, .. }) if *path == cert_file),
        "{err:?}"
    );
}

#[test]
fn client_auth_reaches_the_tls_builder() {
    let err = rejected(Config {
        tls: Some(TlsConfig {
            client_auth: ClientAuth::Required,
            ..insecure_tls()
        }),
        ..dial_only()
    });
    assert!(
        matches!(err, Error::Tls(tls::Error::ClientAuthWithoutIdentity)),
        "{err:?}"
    );
}

// The builder mapping is pure: a TLS section whose files do not exist maps
// onto the builder without touching them, and the further builder call
// still applies; the missing file surfaces only at build().
#[test]
fn the_builder_mapping_touches_no_file() {
    let dir = tempdir().unwrap();
    let ca_certs = dir.path().join("missing-ca");
    let builder = Tcpclv4Builder::from(Config {
        tls: Some(TlsConfig {
            ca_certs: Some(ca_certs.clone()),
            ..Default::default()
        }),
        ..dial_only()
    })
    .transfer_mru(NonZeroU64::new(4096).unwrap());
    let Err(err) = builder.build() else {
        panic!("the missing CA directory must fail the build");
    };
    assert!(
        matches!(&err, Error::Tls(tls::Error::CaCertsMissing { path }) if *path == ca_certs),
        "{err:?}"
    );
}

#[test]
fn required_tls_needs_an_identity_to_listen() {
    // Judged before any listener is bound.
    let err = rejected(Config {
        listeners: Some(vec!["127.0.0.1:0".parse().unwrap()]),
        tls: Some(TlsConfig {
            required: true,
            ..insecure_tls()
        }),
        ..Default::default()
    });
    assert!(matches!(err, Error::RequiredTlsWithoutIdentity), "{err:?}");
}

#[cfg(feature = "serde")]
mod schema {
    use core::num::{NonZeroU32, NonZeroUsize};
    use std::path::PathBuf;

    use hardy_tcpclv4::{ContactTimeout, KeepaliveInterval};
    use serde_json::{Error as JsonError, Value, from_value, json, to_value};

    use super::*;

    fn parse(value: Value) -> Result<Config, JsonError> {
        from_value(value)
    }

    fn every_key() -> Config {
        Config {
            listeners: Some(vec!["127.0.0.1:4556".parse().unwrap()]),
            segment_mru: NonZeroU64::new(8192),
            transfer_mru: NonZeroU64::new(65536),
            max_idle_connections: Some(2),
            max_outstanding_transfers: NonZeroUsize::new(4),
            connection_rate_limit: NonZeroU32::new(8),
            contact_timeout: ContactTimeout::new(30),
            keepalive_interval: Some(KeepaliveInterval::DISABLED),
            tls: Some(TlsConfig {
                required: true,
                ca_certs: Some(PathBuf::from("/etc/hardy/ca")),
                insecure_skip_verify: false,
                identity: Some(Identity {
                    cert_file: PathBuf::from("cert.pem"),
                    key_file: PathBuf::from("key.pem"),
                }),
                client_auth: ClientAuth::Optional,
                server_name: Some("peer.example".to_string()),
            }),
        }
    }

    // The documented kebab-case keys, `private-key-file` alias included,
    // parse into the exact value.
    #[test]
    fn kebab_case_keys_parse() {
        let config = parse(json!({
            "listeners": ["127.0.0.1:4556"],
            "segment-mru": 8192,
            "transfer-mru": 65536,
            "max-idle-connections": 2,
            "max-outstanding-transfers": 4,
            "connection-rate-limit": 8,
            "contact-timeout": 30,
            "keepalive-interval": 0,
            "tls": {
                "required": true,
                "ca-certs": "/etc/hardy/ca",
                "identity": { "cert-file": "cert.pem", "private-key-file": "key.pem" },
                "client-auth": "optional",
                "server-name": "peer.example"
            }
        }))
        .unwrap();
        assert_eq!(config, every_key());
    }

    // A serialized config parses back to itself; an absent keepalive is
    // left out rather than written as the refused `null`.
    #[test]
    fn serialized_config_parses_back() {
        for config in [Config::default(), every_key()] {
            let value = to_value(&config).unwrap();
            assert_eq!(parse(value).unwrap(), config);
        }
    }

    // `keepalive-interval: null` is refused, naming both real spellings;
    // an absent key is the builder default.
    #[test]
    fn null_keepalive_is_refused() {
        let err = parse(json!({ "keepalive-interval": null })).unwrap_err();
        assert!(err.is_data(), "{err:?}");
        assert!(
            err.to_string().contains("use 0 to disable keepalives"),
            "{err}"
        );
        assert_eq!(parse(json!({})).unwrap().keepalive_interval, None);
    }

    // An identity is a pair: a lone certificate or key is refused.
    #[test]
    fn a_lone_identity_half_is_refused() {
        for identity in [
            json!({ "cert-file": "c.pem" }),
            json!({ "key-file": "k.pem" }),
        ] {
            let err = parse(json!({ "tls": { "identity": identity } })).unwrap_err();
            assert!(err.is_data(), "{err:?}");
        }
    }

    // Out-of-range scalars fail at parse: zero for the NonZero fields, and a
    // contact timeout outside RFC 9174's 1 to 60 seconds.
    #[test]
    fn out_of_range_scalars_are_refused() {
        for (key, value) in [
            ("segment-mru", json!(0)),
            ("transfer-mru", json!(0)),
            ("max-outstanding-transfers", json!(0)),
            ("connection-rate-limit", json!(0)),
            ("contact-timeout", json!(0)),
            ("contact-timeout", json!(61)),
        ] {
            let err = parse(json!({ key: value })).unwrap_err();
            assert!(err.is_data(), "{key} = {value}: {err:?}");
        }
    }

    // Unknown keys are refused at every level, so a typo cannot leave a
    // default in force.
    #[test]
    fn unknown_keys_are_refused() {
        for entry in [
            json!({ "address": "127.0.0.1:4556" }),
            json!({ "tls": { "insecure": true } }),
            json!({ "tls": { "identity": { "cert-file": "c", "key-file": "k", "extra": 1 } } }),
        ] {
            let err = parse(entry.clone()).unwrap_err();
            assert!(err.is_data(), "{entry}: {err:?}");
        }
    }
}
