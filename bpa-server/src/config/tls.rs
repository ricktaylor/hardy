#[cfg(any(feature = "tcpclv4", feature = "grpc"))]
use std::path::PathBuf;

#[cfg(feature = "tcpclv4")]
use hardy_tcpclv4::tls;
#[cfg(any(feature = "tcpclv4", feature = "grpc"))]
use serde::{Deserialize, Serialize};

// A certificate and the private key that proves it: only representable as
// a pair.
#[cfg(any(feature = "tcpclv4", feature = "grpc"))]
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Identity {
    // The node's certificate (PEM).
    pub cert_file: PathBuf,

    // The private key (PEM: PKCS#8, PKCS#1, or SEC1) matching `cert-file`.
    // `Config::warn_insecure_keys` checks its file permissions at startup.
    #[serde(alias = "private-key-file")]
    pub key_file: PathBuf,
}

// Client-certificate verification policy for inbound TLS connections
// (mutual TLS): `required` refuses dialers without a certificate chaining
// to `ca-certs`; `optional` verifies a certificate when one is presented
// but accepts dialers without one; `off` never requests one.
#[cfg(any(feature = "tcpclv4", feature = "grpc"))]
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
#[cfg(feature = "tcpclv4")]
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
#[cfg(feature = "tcpclv4")]
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

// The `tls` sub-section of the `grpc` section. This is a listener, so it
// carries only the server-relevant subset of the TLS vocabulary: an
// `identity` to present (required, so "TLS without a certificate" cannot
// be written), and the mutual-TLS knobs. The dial-side keys of the CLA
// `tls` section (`required`, `server-name`, `insecure-skip-verify`) have
// no meaning here and are absent.
#[cfg(feature = "grpc")]
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct GrpcTlsConfig {
    // The server's own certificate and private key, presented to every
    // client.
    pub identity: Identity,

    // Client-certificate verification for inbound connections (mutual
    // TLS): `off` (the default) never requests one, `optional` verifies a
    // presented certificate but accepts dialers without one, `required`
    // refuses dialers without a certificate chaining to `ca-certs`. Any
    // value other than `off` requires `ca-certs`.
    #[serde(default)]
    pub client_auth: ClientAuth,

    // A PEM file of CA certificates (one file, one or more certificates)
    // used to verify client certificates under mutual TLS. Required when
    // `client-auth` is not `off`, ignored otherwise.
    #[serde(default)]
    pub ca_certs: Option<PathBuf>,
}
