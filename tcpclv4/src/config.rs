//! [`Config`]: a [`Tcpclv4`] build described as plain data, for
//! configuration files.
//!
//! Every field mirrors a [`Tcpclv4Builder`] input. An absent (`None`)
//! field leaves the builder default in force, so no default value is
//! restated here; the defaults are documented on the builder methods.
//!
//! [`Tcpclv4Builder::from`] maps a `Config` onto a builder without touching
//! a socket or a file, so the result can be adjusted before
//! [`build()`](Tcpclv4Builder::build), the one effectful step.
//! [`Tcpclv4::try_from`] is the shorthand for both: it binds the listeners
//! and loads the TLS material.
//!
//! With the `serde` feature the types deserialize from the kebab-case
//! schema the Hardy servers read (a `clas:` entry in `hardy-bpa-server`,
//! the `cla` table of `hardy-tcpclv4-server`). Unknown keys are refused, and
//! scalar invariants are carried by the field types (`NonZero` integers,
//! [`ContactTimeout`]), so invalid values fail at parse.

use core::num::{NonZeroU32, NonZeroU64, NonZeroUsize};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use hardy_bpa::cla::Registration;
#[cfg(feature = "serde")]
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    ContactTimeout, KeepaliveInterval, Tcpclv4,
    builder::Tcpclv4Builder,
    error::Error,
    tls::{self, ClientAuth, Tls, TlsBuilder},
};

/// A [`Tcpclv4`] build as plain data; `None` fields leave the builder
/// defaults in force.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(default, deny_unknown_fields, rename_all = "kebab-case")
)]
pub struct Config {
    /// The passive listening elements, one address per listener. `None`
    /// listens on the IANA-registered `[::]:4556`; an empty list disables
    /// listening (dial-only).
    pub listeners: Option<Vec<SocketAddr>>,

    /// Largest acceptable single-segment payload, in bytes.
    pub segment_mru: Option<NonZeroU64>,

    /// Largest acceptable total bundle transfer, in bytes.
    pub transfer_mru: Option<NonZeroU64>,

    /// Idle connections retained per remote address; 0 disables pooling.
    pub max_idle_connections: Option<usize>,

    /// Transfers accepted but not yet resolved with an outcome, per peer.
    /// Bounds the bundles held in memory by in-flight and queued transfers
    /// to each peer.
    pub max_outstanding_transfers: Option<NonZeroUsize>,

    /// Inbound connections accepted per second.
    pub connection_rate_limit: Option<NonZeroU32>,

    /// Seconds to wait for a peer's contact header (RFC 9174 Section 4.2).
    pub contact_timeout: Option<ContactTimeout>,

    /// Keepalive interval in seconds (RFC 9174 Section 4.7); 0 disables
    /// keepalives. `null` is not a spelling of either: it is refused at
    /// parse, so it cannot be mistaken for "disabled".
    // Skipped when `None` on output, so a serialized config re-parses.
    #[cfg_attr(
        feature = "serde",
        serde(
            deserialize_with = "keepalive_refusing_null",
            skip_serializing_if = "Option::is_none"
        )
    )]
    pub keepalive_interval: Option<KeepaliveInterval>,

    /// TLS configuration; `None` means plaintext.
    pub tls: Option<TlsConfig>,
}

/// The `tls` section of a [`Config`]: the inputs of a
/// [`TlsBuilder`], converted with
/// [`Tls::try_from`].
///
/// The shape keeps invalid combinations out of the file: `identity` is
/// one object with two required fields, so a lone certificate or key
/// cannot be written, and `required` lives inside the section, so
/// "require TLS without configuring TLS" cannot be written. The
/// trust-anchor rules are judged by [`TlsBuilder::build`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(default, deny_unknown_fields, rename_all = "kebab-case")
)]
pub struct TlsConfig {
    /// Refuse sessions that do not negotiate TLS.
    ///
    /// Default: `false`.
    pub required: bool,

    /// Directory of PEM CA certificates used to verify peers'
    /// certificates: the standing trust anchor for normal operation.
    pub ca_certs: Option<PathBuf>,

    /// Accept any peer certificate chain with no trust validation
    /// (INSECURE; testing only). The key is deliberately loud and has no
    /// shorter alias: the danger must be visible in the file. Overrides
    /// `ca-certs` when both are set, with a startup warning, and the
    /// ignored certificates are never loaded.
    ///
    /// Default: `false`.
    pub insecure_skip_verify: bool,

    /// The node's own certificate and private key. Required to accept TLS
    /// connections (the listener's TLS server role), and presented to
    /// dialed peers under mutual TLS.
    pub identity: Option<Identity>,

    /// Client-certificate verification for inbound connections (mutual
    /// TLS). Requires `identity` and a `ca-certs` trust anchor.
    ///
    /// Default: [`ClientAuth::Off`].
    pub client_auth: ClientAuth,

    /// SNI override presented when dialing (for certificates issued to
    /// domain names).
    pub server_name: Option<String>,
}

/// A certificate and the private key that proves it: only representable
/// as a pair.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(deny_unknown_fields, rename_all = "kebab-case")
)]
pub struct Identity {
    /// The node's certificate (PEM).
    pub cert_file: PathBuf,

    /// The private key (PEM: PKCS#8, PKCS#1, or SEC1) matching
    /// `cert-file`. `private-key-file` is accepted as an alias.
    #[cfg_attr(feature = "serde", serde(alias = "private-key-file"))]
    pub key_file: PathBuf,
}

impl From<Config> for Tcpclv4Builder {
    /// Maps every set field onto the builder; touches no socket or file.
    fn from(config: Config) -> Self {
        let mut builder = match config.listeners {
            Some(listeners) => listeners
                .into_iter()
                .fold(Tcpclv4::builder(), |builder, address| {
                    builder.listen(address)
                }),
            None => Tcpclv4::builder().listen_default(),
        };
        if let Some(mru) = config.segment_mru {
            builder = builder.segment_mru(mru);
        }
        if let Some(mru) = config.transfer_mru {
            builder = builder.transfer_mru(mru);
        }
        if let Some(limit) = config.max_idle_connections {
            builder = builder.max_idle_connections(limit);
        }
        if let Some(limit) = config.max_outstanding_transfers {
            builder = builder.max_outstanding_transfers(limit);
        }
        if let Some(rate) = config.connection_rate_limit {
            builder = builder.connection_rate_limit(rate);
        }
        if let Some(timeout) = config.contact_timeout {
            builder = builder.contact_timeout(timeout);
        }
        if let Some(interval) = config.keepalive_interval {
            builder = builder.keepalive_interval(interval);
        }
        if let Some(tls) = config.tls {
            builder = builder.tls(tls.into());
        }
        builder
    }
}

impl TryFrom<Config> for Tcpclv4 {
    type Error = Error;

    /// Builds the entity, `Tcpclv4Builder::from(config).build()`: binds
    /// the listeners and loads the TLS material.
    ///
    /// # Errors
    ///
    /// The errors of [`TlsBuilder::build`] (as [`Error::Tls`]) and of
    /// [`Tcpclv4Builder::build`].
    fn try_from(config: Config) -> Result<Self, Self::Error> {
        Tcpclv4Builder::from(config).build()
    }
}

impl TryFrom<Config> for Registration {
    type Error = Error;

    /// Builds the entity, `Tcpclv4Builder::from(config).build()`, and pairs
    /// it with its [`Tcpclv4::cla_init`] declarations, ready for
    /// [`BpaBuilder::cla`](hardy_bpa::builder::BpaBuilder::cla).
    fn try_from(config: Config) -> Result<Self, Self::Error> {
        let cla = Tcpclv4Builder::from(config).build()?;
        let init = cla.cla_init();
        Ok(Self::new(Arc::new(cla), init))
    }
}

impl From<TlsConfig> for TlsBuilder {
    /// Maps every set field onto the builder; touches no file.
    fn from(config: TlsConfig) -> Self {
        let mut builder = Tls::builder()
            .required(config.required)
            .client_auth(config.client_auth);
        if let Some(dir) = config.ca_certs {
            builder = builder.ca_certs(dir);
        }
        if config.insecure_skip_verify {
            builder = builder.dangerous().insecure_skip_verify();
        }
        if let Some(identity) = config.identity {
            builder = builder.identity(identity.cert_file, identity.key_file);
        }
        if let Some(name) = config.server_name {
            builder = builder.server_name(name);
        }
        builder
    }
}

impl TryFrom<TlsConfig> for Tls {
    type Error = tls::Error;

    /// Loads and validates the TLS material,
    /// `TlsBuilder::from(config).build()`.
    ///
    /// # Errors
    ///
    /// The errors of [`TlsBuilder::build`].
    fn try_from(config: TlsConfig) -> Result<Self, Self::Error> {
        TlsBuilder::from(config).build()
    }
}

// An absent key is the builder default and `0` disables keepalives; an
// explicit `null` would read as neither, so it is refused with both
// spellings named.
#[cfg(feature = "serde")]
fn keepalive_refusing_null<'de, D>(deserializer: D) -> Result<Option<KeepaliveInterval>, D::Error>
where
    D: Deserializer<'de>,
{
    match Option::<KeepaliveInterval>::deserialize(deserializer)? {
        None => Err(D::Error::custom(
            "keepalive-interval does not accept null: use 0 to disable keepalives, or omit the key for the default",
        )),
        some => Ok(some),
    }
}
