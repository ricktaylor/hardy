use bytes::Bytes;

use super::{Error, Result};

/// An enumeration of known CLA address types.
///
/// This is used to identify the protocol associated with a `ClaAddress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ClaAddressType {
    /// IPv4 and IPv6 address + port.
    Tcp,
    /// A private address type.
    Private,
}

/// Represents a network address for a specific Convergence Layer Adapter.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ClaAddress {
    /// An TCP address, represented as a standard socket address.
    Tcp(core::net::SocketAddr),
    /// An address for an unknown or custom CLA, containing the type identifier and the raw address bytes.
    #[cfg_attr(feature = "serde", serde(with = "private_addr_serde"))]
    Private(Bytes),
}

#[cfg(feature = "serde")]
mod private_addr_serde {
    use super::Bytes;
    use base64::prelude::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        BASE64_URL_SAFE_NO_PAD.encode(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        let s = String::deserialize(d)?;
        BASE64_URL_SAFE_NO_PAD
            .decode(&s)
            .map(|v| v.into())
            .map_err(serde::de::Error::custom)
    }
}

impl ClaAddress {
    pub fn address_type(&self) -> ClaAddressType {
        match self {
            ClaAddress::Tcp(_) => ClaAddressType::Tcp,
            ClaAddress::Private(_) => ClaAddressType::Private,
        }
    }
}

impl TryFrom<(ClaAddressType, Bytes)> for ClaAddress {
    type Error = Error;

    fn try_from((addr_type, addr): (ClaAddressType, Bytes)) -> Result<Self> {
        match addr_type {
            ClaAddressType::Tcp => Ok(ClaAddress::Tcp(
                String::from_utf8(addr.into())
                    .map_err(|e| Error::Internal(Box::new(e)))?
                    .parse()
                    .map_err(|e| Error::Internal(Box::new(e)))?,
            )),
            ClaAddressType::Private => Ok(ClaAddress::Private(addr)),
        }
    }
}

impl From<ClaAddress> for (ClaAddressType, Bytes) {
    fn from(value: ClaAddress) -> Self {
        match value {
            ClaAddress::Tcp(socket_addr) => (
                ClaAddressType::Tcp,
                socket_addr.to_string().into_bytes().into(),
            ),
            ClaAddress::Private(bytes) => (ClaAddressType::Private, bytes),
        }
    }
}

impl core::fmt::Display for ClaAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ClaAddress::Tcp(socket_addr) => write!(f, "tcp:{socket_addr}"),
            ClaAddress::Private(bytes) => {
                write!(f, "private:{bytes:02x?}")
            }
        }
    }
}
