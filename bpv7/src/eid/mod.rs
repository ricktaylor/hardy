use alloc::{
    borrow::Cow,
    boxed::Box,
    format,
    string::{String, ToString},
};
use core::fmt;

use hardy_cbor::{
    // Aliased: collides with this module's own `Error`.
    decode::{Error as CborError, parse_value, skip_value},
    encode::{Encoder, Raw, ToCbor},
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
// Encode set matching RFC 3986 unreserved characters (keeps alphanumerics, -, _, ., ~)
const URI_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

mod error;
mod parse;

pub use self::error::{Error, Result};

/// A fully qualified node number in the `ipn` EID scheme (RFC 9171 Section 4.2.5.1.2).
///
/// Encoded as `ipn:<allocator_id>.<node_number>.<service_number>`, where
/// a zero `allocator_id` is omitted from the display form.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct IpnNodeId {
    /// The allocator identifier. Zero indicates the default allocator.
    pub allocator_id: u32,
    /// The node number within the allocator's namespace.
    pub node_number: u32,
}

impl From<IpnNodeId> for Eid {
    fn from(value: IpnNodeId) -> Self {
        Eid::Ipn {
            fqnn: value,
            service_number: 0,
        }
    }
}

impl From<NodeId> for String {
    fn from(value: NodeId) -> Self {
        value.to_string()
    }
}

impl fmt::Display for IpnNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.allocator_id == 0 {
            write!(f, "ipn:{}.0", self.node_number)
        } else {
            write!(f, "ipn:{}.{}.0", self.allocator_id, self.node_number)
        }
    }
}

/// A node identifier in the `dtn` EID scheme (RFC 9171 Section 4.2.5.1.3).
///
/// Displayed as `dtn://<node_name>/`.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct DtnNodeId {
    /// The authority component of the `dtn` URI.
    pub node_name: Box<str>,
}

impl From<DtnNodeId> for Eid {
    fn from(node_name: DtnNodeId) -> Self {
        Eid::Dtn {
            node_name,
            service_name: "".into(),
        }
    }
}

impl fmt::Display for DtnNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dtn://{}/", self.node_name)
    }
}

/// The node identity component of an [`Eid`], without any service demultiplexer.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(into = "String", try_from = "Cow<str>")
)]
pub enum NodeId {
    /// The local node (self-referential sentinel).
    LocalNode,
    /// An `ipn`-scheme node (RFC 9171 Section 4.2.5.1.2).
    Ipn(IpnNodeId),
    /// A `dtn`-scheme node (RFC 9171 Section 4.2.5.1.3).
    Dtn(DtnNodeId),
}

impl TryFrom<Eid> for NodeId {
    type Error = Error;

    fn try_from(value: Eid) -> core::result::Result<Self, Self::Error> {
        match value {
            Eid::LocalNode(0) => Ok(NodeId::LocalNode),
            Eid::LegacyIpn {
                fqnn,
                service_number,
            }
            | Eid::Ipn {
                fqnn,
                service_number,
            } if service_number == 0 => Ok(NodeId::Ipn(fqnn)),
            Eid::Dtn {
                node_name,
                service_name,
            } if service_name.is_empty() => Ok(NodeId::Dtn(node_name)),
            _ => Err(Error::InvalidNodeId),
        }
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeId::LocalNode => f.write_str("ipn:!.0"),
            NodeId::Ipn(ipn) => {
                write!(f, "{ipn}")
            }
            NodeId::Dtn(dtn) => write!(f, "{dtn}"),
        }
    }
}

/// The service demultiplexer component of an [`Eid`].
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(untagged)
)]
pub enum Service {
    /// A numeric service number from the `ipn` scheme.
    Ipn(u32),
    /// A named service path from the `dtn` scheme.
    Dtn(Box<str>),
}

impl fmt::Display for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Service::Ipn(n) => write!(f, "{n}"),
            Service::Dtn(s) => write!(f, "{s}"),
        }
    }
}

/// The scheme-specific part of an unrecognised-scheme EID: guaranteed to
/// be exactly one well-formed CBOR data item, so it can be re-emitted
/// verbatim and always displayed.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnknownSsp(Box<[u8]>);

impl UnknownSsp {
    /// Fails with [`Error::InvalidSsp`] unless `data` is exactly one
    /// well-formed CBOR data item.
    pub fn new(data: Box<[u8]>) -> Result<Self> {
        let (_, len) = skip_value(&data, 16).map_err(Error::InvalidCBOR)?;
        if len != data.len() {
            // Trailing bytes after the first data item.
            return Err(Error::InvalidSsp);
        }
        Ok(Self(data))
    }

    /// The validated single-item CBOR encoding of the SSP.
    pub fn as_cbor(&self) -> &[u8] {
        &self.0
    }
}

/// A Bundle Protocol Endpoint Identifier (EID) as defined in RFC 9171 Section 4.2.5.1.
#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(into = "String", try_from = "Cow<str>")
)]
pub enum Eid {
    /// The null endpoint `dtn:none`, indicating no endpoint (RFC 9171 Section 4.2.5.1.1).
    #[default]
    Null,
    /// A service on the local node, used as a self-referential sentinel.
    LocalNode(u32),
    /// An `ipn`-scheme EID decoded from the legacy two-element CBOR array encoding.
    LegacyIpn {
        /// The fully qualified node number.
        fqnn: IpnNodeId,
        /// The service number demultiplexer.
        service_number: u32,
    },
    /// An `ipn`-scheme EID (RFC 9171 Section 4.2.5.1.2).
    Ipn {
        /// The fully qualified node number.
        fqnn: IpnNodeId,
        /// The service number demultiplexer.
        service_number: u32,
    },
    /// A `dtn`-scheme EID (RFC 9171 Section 4.2.5.1.3).
    Dtn {
        /// The node authority component.
        node_name: DtnNodeId,
        /// The service path demultiplexer.
        service_name: Box<str>,
    },
    /// An EID with an unrecognised scheme code, preserved as raw CBOR bytes.
    Unknown {
        /// The numeric scheme code.
        scheme: u64,
        /// The scheme-specific content: one well-formed CBOR data item.
        ssp: UnknownSsp,
    },
}

impl Eid {
    /// Returns `true` the Eid is the 'null endpoint' as defined in RFC 9171.
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, Eid::Null)
    }

    /// Returns the service component of this EID, or `None` if it is the admin endpoint or null.
    pub fn service(&self) -> Option<Service> {
        match self {
            Eid::LocalNode(service_number)
            | Eid::LegacyIpn { service_number, .. }
            | Eid::Ipn { service_number, .. } => {
                (*service_number != 0).then_some(Service::Ipn(*service_number))
            }
            Eid::Dtn { service_name, .. } => {
                (!service_name.is_empty()).then_some(Service::Dtn(service_name.clone()))
            }
            _ => None,
        }
    }

    /// Returns `true` if this EID refers to the administrative endpoint of a node.
    #[inline]
    pub fn is_admin_endpoint(&self) -> bool {
        match self {
            Eid::LocalNode(service_number)
            | Eid::LegacyIpn { service_number, .. }
            | Eid::Ipn { service_number, .. } => *service_number == 0,
            Eid::Dtn { service_name, .. } => service_name.is_empty(),
            _ => false,
        }
    }

    /// Converts this EID into a [`NodeId`], discarding the service component.
    pub fn try_to_node_id(self) -> Result<NodeId> {
        match self {
            Eid::LocalNode(_) => Ok(NodeId::LocalNode),
            Eid::LegacyIpn { fqnn, .. } | Eid::Ipn { fqnn, .. } => Ok(NodeId::Ipn(fqnn)),
            Eid::Dtn { node_name, .. } => Ok(NodeId::Dtn(node_name)),
            _ => Err(Error::InvalidNodeId),
        }
    }

    /// Returns the [`NodeId`] for this EID without consuming it.
    pub fn to_node_id(&self) -> Result<NodeId> {
        match self {
            Eid::LocalNode(_) => Ok(NodeId::LocalNode),
            Eid::LegacyIpn { fqnn, .. } | Eid::Ipn { fqnn, .. } => Ok(NodeId::Ipn(*fqnn)),
            Eid::Dtn { node_name, .. } => Ok(NodeId::Dtn(node_name.clone())),
            _ => Err(Error::InvalidNodeId),
        }
    }
}

impl From<NodeId> for Eid {
    fn from(value: NodeId) -> Self {
        match value {
            NodeId::LocalNode => Eid::LocalNode(0),
            NodeId::Ipn(node_id) => node_id.into(),
            NodeId::Dtn(node_id) => node_id.into(),
        }
    }
}

impl TryFrom<(NodeId, Service)> for Eid {
    type Error = Error;

    fn try_from(value: (NodeId, Service)) -> core::result::Result<Self, Self::Error> {
        match value {
            (NodeId::LocalNode, Service::Ipn(service_number)) => Ok(Eid::LocalNode(service_number)),
            (NodeId::Ipn(fqnn), Service::Ipn(service_number)) => Ok(Eid::Ipn {
                fqnn,
                service_number,
            }),
            (NodeId::Dtn(node_name), Service::Dtn(service_name)) => Ok(Eid::Dtn {
                node_name,
                service_name,
            }),
            _ => Err(Error::MismatchedService),
        }
    }
}

impl ToCbor for Eid {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        match self {
            Eid::LocalNode(service_number) => encoder.emit(&(2, &(u32::MAX, *service_number))),
            Eid::Null => encoder.emit(&(1, 0)),
            Eid::Dtn {
                node_name,
                service_name,
            } => encoder.emit(&(
                1,
                format!(
                    "//{}/{service_name}",
                    percent_encode(node_name.node_name.as_bytes(), URI_ENCODE_SET)
                ),
            )),
            Eid::LegacyIpn {
                fqnn: fqdn,
                service_number,
            } => encoder.emit(&(
                2,
                &(
                    (((fqdn.allocator_id as u64) << 32) | fqdn.node_number as u64),
                    service_number,
                ),
            )),
            Eid::Ipn {
                fqnn: fqdn,
                service_number,
            } => {
                if fqdn.allocator_id == 0 {
                    encoder.emit(&(2, &[fqdn.node_number, *service_number]))
                } else {
                    encoder.emit(&(2, &[fqdn.allocator_id, fqdn.node_number, *service_number]))
                }
            }
            Eid::Unknown { scheme, ssp } => encoder.emit(&(scheme, Raw(ssp.as_cbor()))),
        }
    }
}

impl From<Eid> for String {
    fn from(value: Eid) -> Self {
        value.to_string()
    }
}

impl fmt::Display for Eid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Eid::Null => f.write_str("dtn:none"),
            Eid::LocalNode(service_number) => {
                write!(f, "ipn:!.{service_number}")
            }
            Eid::LegacyIpn {
                fqnn: fqdn,
                service_number,
            }
            | Eid::Ipn {
                fqnn: fqdn,
                service_number,
            } if fqdn.allocator_id == 0 => write!(f, "ipn:{}.{service_number}", fqdn.node_number),
            Eid::LegacyIpn {
                fqnn: fqdn,
                service_number,
            }
            | Eid::Ipn {
                fqnn: fqdn,
                service_number,
            } => write!(
                f,
                "ipn:{}.{}.{service_number}",
                fqdn.allocator_id, fqdn.node_number
            ),
            Eid::Dtn {
                node_name,
                service_name,
            } => {
                write!(
                    f,
                    "dtn://{}/{service_name}",
                    percent_encode(node_name.node_name.as_bytes(), URI_ENCODE_SET)
                )
            }
            Eid::Unknown { scheme, ssp } => {
                match parse_value(ssp.as_cbor(), |mut value, _, _| {
                    let r = write!(f, "unknown({scheme}):{value:?}");
                    value.skip(16)?;
                    Ok::<_, CborError>(r)
                }) {
                    Ok((r, _)) => r,
                    // The SSP is well-formed CBOR by construction, but need
                    // not be decodable (e.g. a text string that is not valid
                    // UTF-8). Display must stay total: returning `fmt::Error`
                    // on a healthy stream aborts `format!`.
                    Err(e) => write!(f, "unknown({scheme}):error: {e:?}"),
                }
            }
        }
    }
}
