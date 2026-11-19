use core::{fmt, net::SocketAddr};

use super::{Error, Result};
use crate::Bytes;

/// The protocol an [`Address`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AddressType {
    /// TCP, over IPv4 or IPv6.
    Tcp,
    /// Anything else, which the BPA carries without interpreting.
    Private,
}

/// Where a convergence layer reaches a peer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Address {
    /// A TCP endpoint.
    Tcp(SocketAddr),
    /// A convergence layer's own address, opaque to the BPA.
    #[cfg_attr(feature = "serde", serde(with = "private_serde"))]
    Private(Bytes),
}

impl Address {
    /// The protocol this address belongs to.
    pub fn address_type(&self) -> AddressType {
        match self {
            Address::Tcp(_) => AddressType::Tcp,
            Address::Private(_) => AddressType::Private,
        }
    }
}

impl TryFrom<(AddressType, Bytes)> for Address {
    type Error = Error;

    fn try_from((addr_type, addr): (AddressType, Bytes)) -> Result<Self> {
        match addr_type {
            AddressType::Tcp => Ok(Address::Tcp(
                String::from_utf8(addr.into())
                    .map_err(|e| Error::Internal(Box::new(e)))?
                    .parse()
                    .map_err(|e| Error::Internal(Box::new(e)))?,
            )),
            AddressType::Private => Ok(Address::Private(addr)),
        }
    }
}

impl From<Address> for (AddressType, Bytes) {
    fn from(value: Address) -> Self {
        match value {
            Address::Tcp(socket_addr) => (
                AddressType::Tcp,
                socket_addr.to_string().into_bytes().into(),
            ),
            Address::Private(bytes) => (AddressType::Private, bytes),
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Address::Tcp(socket_addr) => write!(f, "tcp:{socket_addr}"),
            Address::Private(bytes) => write!(f, "private:{bytes:02x?}"),
        }
    }
}

/// `Bytes` has no `serde` impl of its own, and a private address is arbitrary
/// bytes, so it travels base64 to survive text formats.
#[cfg(feature = "serde")]
mod private_serde {
    use base64::prelude::*;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::Bytes;

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

#[cfg(test)]
mod tests {
    use super::*;

    // An `Address` survives the `(AddressType, Bytes)` wire pair unchanged.
    #[test]
    fn test_address_parsing() {
        // TCP address: parse from string representation
        let tcp_addr: SocketAddr = "192.168.1.1:4556".parse().unwrap();
        let cla_addr = Address::Tcp(tcp_addr);
        assert_eq!(cla_addr.address_type(), AddressType::Tcp);

        // Round-trip through the wire pair.
        let (addr_type, bytes): (AddressType, Bytes) = cla_addr.clone().into();
        assert_eq!(addr_type, AddressType::Tcp);
        let recovered = Address::try_from((addr_type, bytes)).unwrap();
        assert_eq!(recovered, cla_addr);

        // IPv6 TCP address
        let tcp_v6: SocketAddr = "[::1]:4556".parse().unwrap();
        let cla_v6 = Address::Tcp(tcp_v6);
        let (t, b): (AddressType, Bytes) = cla_v6.clone().into();
        let recovered = Address::try_from((t, b)).unwrap();
        assert_eq!(recovered, cla_v6);

        // Private address
        let private_data = Bytes::from_static(b"\x01\x02\x03\x04");
        let private_addr = Address::Private(private_data.clone());
        assert_eq!(private_addr.address_type(), AddressType::Private);

        let (t, b): (AddressType, Bytes) = private_addr.clone().into();
        assert_eq!(t, AddressType::Private);
        let recovered = Address::try_from((t, b)).unwrap();
        assert_eq!(recovered, private_addr);

        // Invalid TCP bytes should error
        let bad_bytes = Bytes::from_static(b"not-a-socket-addr");
        let result = Address::try_from((AddressType::Tcp, bad_bytes));
        assert!(result.is_err());
    }
}
