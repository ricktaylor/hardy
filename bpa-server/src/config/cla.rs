#[cfg(feature = "tcpclv4")]
use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
#[cfg(feature = "tcpclv4")]
use std::net::SocketAddr;
#[cfg(feature = "tcpclv4")]
use std::path::PathBuf;

#[cfg(feature = "tcpclv4")]
use hardy_tcpclv4::{ContactTimeout, KeepaliveInterval, tls};
use serde::{Deserialize, Serialize};

// `deny_unknown_fields` does not compose with `flatten`, but strictness
// holds anyway: every key that is not `name` or `policy` is forwarded to
// the flattened `ClaType`, whose payloads are strict for known types.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClaConfig {
    pub name: String,
    #[serde(flatten)]
    pub cla_type: ClaType,
    #[serde(default)]
    pub policy: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ClaType {
    #[cfg(feature = "tcpclv4")]
    #[serde(rename = "tcpclv4")]
    TcpClv4(Tcpclv4Config),

    #[cfg(feature = "file-cla")]
    #[serde(rename = "file-cla")]
    File(hardy_file_cla::Config),

    #[serde(untagged)]
    Other {
        #[serde(rename = "type")]
        cla_type: String,
        #[serde(flatten)]
        config: serde_json::Value,
    },
}

// Unknown CLA types are tolerated (`Other`, ignored with a warning at
// assembly) so a config can name extension CLAs this binary was not built
// with, but a known type with a malformed payload must fail loudly. A
// derived untagged fallback cannot tell the two apart: it swallows the
// payload's parse error and ignores the whole entry, so the dispatch on
// `type` is by hand.
impl<'de> Deserialize<'de> for ClaType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut entry = serde_json::Map::deserialize(deserializer)?;
        let cla_type = match entry.remove("type") {
            Some(serde_json::Value::String(cla_type)) => cla_type,
            Some(_) => return Err(serde::de::Error::custom("type must be a string")),
            None => return Err(serde::de::Error::missing_field("type")),
        };
        let config = serde_json::Value::Object(entry);
        match cla_type.as_str() {
            #[cfg(feature = "tcpclv4")]
            "tcpclv4" => serde_json::from_value(config)
                .map(Self::TcpClv4)
                .map_err(serde::de::Error::custom),
            #[cfg(feature = "file-cla")]
            "file-cla" => serde_json::from_value(config)
                .map(Self::File)
                .map_err(serde::de::Error::custom),
            _ => Ok(Self::Other { cla_type, config }),
        }
    }
}

// `null` was an earlier schema's disabled spelling for the keepalive
// interval; refuse it rather than silently re-enable keepalives. Delete
// after the migration window: the library's serde impl then applies
// (absent means the builder default, `0` disables).
#[cfg(feature = "tcpclv4")]
fn keepalive_refusing_null<'de, D>(deserializer: D) -> Result<Option<KeepaliveInterval>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<KeepaliveInterval>::deserialize(deserializer)? {
        None => Err(serde::de::Error::custom(
            "keepalive-interval does not accept null: use 0 to disable keepalives, or omit the key for the default",
        )),
        some => Ok(some),
    }
}

// A certificate and the private key that proves it: only representable as
// a pair.
#[cfg(feature = "tcpclv4")]
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
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
#[cfg(feature = "tcpclv4")]
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
// default value is restated here. Scalar invariants are carried by the
// schema types (`NonZero` integers, the `contact-timeout` adapter) and
// rejected at parse; the assembly in `BpaServer::new` maps the file
// surface onto the builder, naming config keys in the errors that
// remain. Unknown keys are refused (`deny_unknown_fields`) with the
// known keys listed, so a removed key (the old single-listener
// `address`) or a typo cannot silently leave a default listener in
// force.
#[cfg(feature = "tcpclv4")]
#[derive(Default, Serialize, Deserialize, Debug)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Tcpclv4Config {
    // The passive listening elements, one address per listener; absent
    // listens on the IANA-registered `[::]:4556`, and an empty list
    // disables listening (dial-only).
    pub listeners: Option<Vec<SocketAddr>>,

    // Largest acceptable single-segment payload, in bytes; zero is
    // rejected at parse.
    pub segment_mru: Option<NonZeroU64>,

    // Largest acceptable total bundle transfer, in bytes; zero is
    // rejected at parse.
    pub transfer_mru: Option<NonZeroU64>,

    // Idle connections retained per remote address; 0 disables pooling.
    pub max_idle_connections: Option<usize>,

    // Transfers accepted but not yet resolved with an outcome, per peer;
    // zero is rejected at parse. Bounds the bundles held in memory by
    // in-flight and queued transfers to each peer.
    pub max_outstanding_transfers: Option<NonZeroUsize>,

    // Inbound connections accepted per second; zero is rejected at parse.
    pub connection_rate_limit: Option<NonZeroU32>,

    // Seconds to wait for a peer's contact header: 1 to 60 (RFC 9174
    // Section 4.2); out-of-range values are rejected at parse.
    pub contact_timeout: Option<ContactTimeout>,

    // Keepalive interval in seconds (RFC 9174 Section 4.7); 0 disables
    // keepalives.
    #[serde(deserialize_with = "keepalive_refusing_null")]
    pub keepalive_interval: Option<KeepaliveInterval>,

    // TLS configuration; absent means plaintext.
    pub tls: Option<TlsConfig>,
}
