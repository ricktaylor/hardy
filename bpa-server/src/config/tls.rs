use std::path::PathBuf;

use hardy_tcpclv4::tls;
use serde::{Deserialize, Serialize};

use super::owner_only_key_file;

// A certificate and the private key that proves it: only representable as
// a pair.
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Identity {
    // The node's certificate (PEM).
    pub cert_file: PathBuf,

    // The private key (PEM: PKCS#8, PKCS#1, or SEC1) matching `cert-file`.
    // A group/other-accessible key file is refused at parse.
    #[serde(alias = "private-key-file", deserialize_with = "owner_only_key_file")]
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

// The `tls` section of a tcpclv4 CLA entry. The serde layer is strict
// and flat, mirroring what the operator types; `identity` is
// one object with two required fields, so a lone certificate or key
// cannot be written, and `required` lives inside the section, so "require
// TLS without configuring TLS" cannot be written. The trust-anchor rules
// are judged by the library at build time, with errors in the config's
// own vocabulary.
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Tcpclv4TlsConfig {
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
