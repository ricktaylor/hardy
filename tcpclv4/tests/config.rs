//! `config::Config` converts into a built `Tcpclv4`. Each conversion test
//! proves one field reaches the builder, through an outcome the public API
//! exposes (the declared receive limit, or the typed error the builder
//! raises for that input).

use core::num::NonZeroU64;
use std::net::TcpListener;

use hardy_tcpclv4::{
    Error, Tcpclv4,
    config::{Config, Identity, TlsConfig},
    tls::{self, ClientAuth},
};

// Binds nothing, so the conversion touches no socket.
fn dial_only() -> Config {
    Config {
        listeners: Some(Vec::new()),
        ..Default::default()
    }
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
    let cla = Tcpclv4::try_from(Config {
        transfer_mru: Some(mru),
        ..dial_only()
    })
    .expect("a dial-only plaintext build cannot fail");
    assert_eq!(cla.cla_init().max_bundle_size, Some(mru));
}

#[test]
fn listeners_are_bound() {
    // The held socket makes the bind fail, naming the configured address.
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = held.local_addr().unwrap();
    let err = Tcpclv4::try_from(Config {
        listeners: Some(vec![address]),
        ..Default::default()
    })
    .unwrap_err();
    assert!(
        matches!(err, Error::BindListener { address: bound, .. } if bound == address),
        "{err:?}"
    );
}

#[test]
fn tls_requires_a_trust_anchor() {
    let err = Tcpclv4::try_from(Config {
        tls: Some(TlsConfig::default()),
        ..dial_only()
    })
    .unwrap_err();
    assert!(
        matches!(err, Error::Tls(tls::Error::NoTrustAnchor)),
        "{err:?}"
    );
}

#[test]
fn insecure_skip_verify_is_a_trust_anchor() {
    Tcpclv4::try_from(Config {
        tls: Some(insecure_tls()),
        ..dial_only()
    })
    .expect("insecure-skip-verify stands in for ca-certs");
}

#[test]
fn ca_certs_are_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let ca_certs = dir.path().join("missing-ca");
    let err = Tcpclv4::try_from(Config {
        tls: Some(TlsConfig {
            ca_certs: Some(ca_certs.clone()),
            ..Default::default()
        }),
        ..dial_only()
    })
    .unwrap_err();
    assert!(
        matches!(&err, Error::Tls(tls::Error::CaCertsMissing { path }) if *path == ca_certs),
        "{err:?}"
    );
}

#[test]
fn identity_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let cert_file = dir.path().join("missing-cert.pem");
    let err = Tcpclv4::try_from(Config {
        tls: Some(TlsConfig {
            identity: Some(Identity {
                cert_file: cert_file.clone(),
                key_file: dir.path().join("missing-key.pem"),
            }),
            ..insecure_tls()
        }),
        ..dial_only()
    })
    .unwrap_err();
    assert!(
        matches!(&err, Error::Tls(tls::Error::LoadCertificate { path, .. }) if *path == cert_file),
        "{err:?}"
    );
}

#[test]
fn client_auth_reaches_the_tls_builder() {
    let err = Tcpclv4::try_from(Config {
        tls: Some(TlsConfig {
            client_auth: ClientAuth::Required,
            ..insecure_tls()
        }),
        ..dial_only()
    })
    .unwrap_err();
    assert!(
        matches!(err, Error::Tls(tls::Error::ClientAuthWithoutIdentity)),
        "{err:?}"
    );
}

#[test]
fn required_tls_needs_an_identity_to_listen() {
    // Judged before any listener is bound.
    let err = Tcpclv4::try_from(Config {
        listeners: Some(vec!["127.0.0.1:0".parse().unwrap()]),
        tls: Some(TlsConfig {
            required: true,
            ..insecure_tls()
        }),
        ..Default::default()
    })
    .unwrap_err();
    assert!(matches!(err, Error::RequiredTlsWithoutIdentity), "{err:?}");
}

#[cfg(feature = "serde")]
mod schema {
    use core::num::{NonZeroU32, NonZeroUsize};
    use std::path::PathBuf;

    use hardy_tcpclv4::{ContactTimeout, KeepaliveInterval};
    use serde_json::json;

    use super::*;

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
        let config: Config = serde_json::from_value(json!({
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
            let value = serde_json::to_value(&config).unwrap();
            assert_eq!(serde_json::from_value::<Config>(value).unwrap(), config);
        }
    }
}
