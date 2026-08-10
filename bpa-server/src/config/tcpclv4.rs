use core::num::{NonZeroU32, NonZeroU64};
use std::net::SocketAddr;
use std::path::PathBuf;

use hardy_tcpclv4::{ContactTimeout, KeepaliveInterval, tls};
use serde::{Deserialize, Serialize};

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

// The `tls` section of a tcpclv4 CLA entry. The serde layer stays
// permissive and flat, mirroring what the operator types; `identity` is
// one object with two required fields, so a lone certificate or key
// cannot be written, and `required` lives inside the section, so "require
// TLS without configuring TLS" cannot be written. The trust-anchor rules
// are judged by the library at build time, with errors in the config's
// own vocabulary.
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
pub struct TlsConfig {
    // Refuse sessions that do not negotiate TLS. Default: `false`.
    pub required: bool,

    // Directory of PEM CA certificates used to verify peers'
    // certificates: the standing trust anchor for normal operation.
    pub ca_certs: Option<PathBuf>,

    // Accept any peer certificate chain with no trust validation
    // (INSECURE; testing only). The key is deliberately loud and has no
    // shorter alias: the danger must be visible in the file. Overrides
    // `ca-certs` when both are set, so a debug session is one line to
    // flip; the override is named in a startup warning, and the ignored
    // bundle is never loaded.
    pub insecure_skip_verify: bool,

    // The node's own certificate and private key. Required to accept TLS
    // connections (the listener's TLS server role), and presented to
    // dialed peers under mutual TLS.
    pub identity: Option<Identity>,

    // Client-certificate verification for inbound connections (mutual
    // TLS). Requires `identity` and a `ca-certs` trust anchor.
    pub client_auth: ClientAuth,

    // SNI override presented when dialing (for certificates issued to
    // domain names).
    pub server_name: Option<String>,
}

// The config-file mirror of the `hardy_tcpclv4` builder inputs, the
// payload of a `type: "tcpclv4"` entry in the `clas` list (kept in sync
// with the flattened mirror in tcpclv4-server/src/config.rs). Absent keys
// stay `None` and leave the corresponding builder default in force, so no
// default value is restated here; `build` maps the file surface onto the
// builder, naming config keys in every error.
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    // The local address to listen on for incoming connections; absent
    // listens on the IANA-registered `[::]:4556`.
    pub address: Option<SocketAddr>,

    // Largest acceptable single-segment payload, in bytes; must be
    // greater than zero.
    pub segment_mru: Option<u64>,

    // Largest acceptable total bundle transfer, in bytes; must be greater
    // than zero.
    pub transfer_mru: Option<u64>,

    // Idle connections retained per remote address; 0 disables pooling.
    pub max_idle_connections: Option<usize>,

    // Inbound connections accepted per second; must be greater than zero.
    pub connection_rate_limit: Option<u32>,

    // Seconds to wait for a peer's contact header: 1 to 60 (RFC 9174
    // Section 4.2).
    pub contact_timeout: Option<u16>,

    // Keepalive interval in seconds; 0 disables keepalives.
    pub keepalive_interval: Option<u16>,

    // TLS configuration; absent means plaintext.
    pub tls: Option<TlsConfig>,
}

impl Config {
    // Reconciles the config-file surface into builder calls: absent keys
    // leave the builder defaults in force, and contradictions are reported
    // with config-key names before any file is touched. The TLS material
    // is loaded by `TlsBuilder::build`, and the listeners are bound inside
    // `Tcpclv4Builder::build`.
    pub fn build(&self) -> anyhow::Result<hardy_tcpclv4::Tcpclv4> {
        let mut builder = hardy_tcpclv4::Tcpclv4::builder();

        builder = match self.address {
            Some(address) => builder.listen(address),
            None => builder.listen_default(),
        };
        if let Some(mru) = self.segment_mru {
            builder = builder.segment_mru(
                NonZeroU64::new(mru)
                    .ok_or_else(|| anyhow::anyhow!("segment-mru must be greater than zero"))?,
            );
        }
        if let Some(mru) = self.transfer_mru {
            builder =
                builder
                    .transfer_mru(NonZeroU64::new(mru).ok_or_else(|| {
                        anyhow::anyhow!("transfer-mru must be greater than zero")
                    })?);
        }
        if let Some(limit) = self.max_idle_connections {
            builder = builder.max_idle_connections(limit);
        }
        if let Some(rate) = self.connection_rate_limit {
            builder = builder.connection_rate_limit(NonZeroU32::new(rate).ok_or_else(|| {
                anyhow::anyhow!("connection-rate-limit must be greater than zero")
            })?);
        }
        if let Some(seconds) = self.contact_timeout {
            builder = builder.contact_timeout(ContactTimeout::new(seconds).ok_or_else(|| {
                anyhow::anyhow!(
                    "contact-timeout must be between 1 and 60 seconds (RFC 9174 Section 4.2)"
                )
            })?);
        }
        // An explicit 0 disables keepalives
        builder = match self.keepalive_interval.map(KeepaliveInterval::new) {
            None => builder,
            Some(None) => builder.no_keepalive(),
            Some(Some(interval)) => builder.keepalive_interval(interval),
        };

        if let Some(tls_config) = &self.tls {
            let mut tls_builder = tls::Tls::builder().required(tls_config.required);

            if let Some(dir) = &tls_config.ca_certs {
                tls_builder = tls_builder.ca_certs(dir.clone());
            }
            if tls_config.insecure_skip_verify {
                tls_builder = tls_builder.dangerous().insecure_skip_verify();
            }
            if let Some(identity) = &tls_config.identity {
                tls_builder =
                    tls_builder.identity(identity.cert_file.clone(), identity.key_file.clone());
            }
            tls_builder = tls_builder.client_auth(tls_config.client_auth.into());
            if let Some(name) = &tls_config.server_name {
                tls_builder = tls_builder.server_name(name.clone());
            }

            builder = builder.tls(tls_builder.build()?);
        }

        Ok(builder.build()?)
    }
}
