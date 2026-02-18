use super::*;
use alloc::borrow::Cow;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
use thiserror::Error;

// Encode set matching RFC 3986 unreserved characters (keeps alphanumerics, -, _, ., ~)
const URI_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

mod error;
mod parse;

pub use error::Error;

#[cfg(test)]
mod str_tests;

#[cfg(test)]
mod cbor_tests;

#[cfg(test)]
mod roundtrip_tests;

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct IpnNodeId {
    pub allocator_id: u32,
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

impl core::fmt::Display for IpnNodeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ipn:{}.{}.0", self.allocator_id, self.node_number)
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct DtnNodeId {
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

impl core::fmt::Display for DtnNodeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "dtn://{}/", self.node_name)
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(into = "String", try_from = "Cow<str>")
)]
pub enum NodeId {
    LocalNode,
    Ipn(IpnNodeId),
    Dtn(DtnNodeId),
}

impl TryFrom<Eid> for NodeId {
    type Error = Error;

    fn try_from(value: Eid) -> Result<Self, Self::Error> {
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

impl core::fmt::Display for NodeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NodeId::LocalNode => f.write_str("ipn:!.0"),
            NodeId::Ipn(ipn) => {
                if ipn.allocator_id == 0 {
                    write!(f, "ipn:{}.0", ipn.node_number)
                } else {
                    write!(f, "ipn:{}.{}.0", ipn.allocator_id, ipn.node_number)
                }
            }
            NodeId::Dtn(dtn) => write!(f, "dtn://{}/", dtn.node_name),
        }
    }
}

pub type IpnServiceNumber = u32;
pub type DtnServiceName = Box<str>;

#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(untagged)
)]
pub enum Service {
    Ipn(IpnServiceNumber),
    Dtn(DtnServiceName),
}

impl core::fmt::Display for Service {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Service::Ipn(n) => write!(f, "{n}"),
            Service::Dtn(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(into = "String", try_from = "Cow<str>")
)]
pub enum Eid {
    #[default]
    Null,
    LocalNode(IpnServiceNumber),
    LegacyIpn {
        fqnn: IpnNodeId,
        service_number: IpnServiceNumber,
    },
    Ipn {
        fqnn: IpnNodeId,
        service_number: IpnServiceNumber,
    },
    Dtn {
        node_name: DtnNodeId,
        service_name: DtnServiceName,
    },
    Unknown {
        scheme: u64,
        data: Box<[u8]>,
    },
}

impl Eid {
    /// Returns `true` the Eid is the 'null endpoint' as defined in RFC 9171.
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, Eid::Null)
    }

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

    pub fn try_to_node_id(self) -> Result<NodeId, Error> {
        match self {
            Eid::LocalNode(_) => Ok(NodeId::LocalNode),
            Eid::LegacyIpn { fqnn, .. } | Eid::Ipn { fqnn, .. } => Ok(NodeId::Ipn(fqnn)),
            Eid::Dtn { node_name, .. } => Ok(NodeId::Dtn(node_name)),
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

    fn try_from(value: (NodeId, Service)) -> Result<Self, Self::Error> {
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

impl hardy_cbor::encode::ToCbor for Eid {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
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
            Eid::Unknown { scheme, data } => encoder.emit(&(scheme, hardy_cbor::encode::Raw(data))),
        }
    }
}

#[derive(Error, Debug)]
enum DisplayError {
    #[error(transparent)]
    Decode(#[from] hardy_cbor::decode::Error),

    #[error(transparent)]
    Fmt(#[from] core::fmt::Error),
}

impl From<Eid> for String {
    fn from(value: Eid) -> Self {
        value.to_string()
    }
}

impl core::fmt::Display for Eid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
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
            Eid::Unknown { scheme, data } => {
                let r = hardy_cbor::decode::parse_value(data, |mut value, _, _| {
                    write!(f, "unknown({scheme}):{value:?}").map_err(Into::<DisplayError>::into)?;
                    value.skip(16).map_err(Into::<DisplayError>::into)
                });
                match r {
                    Ok(_) => Ok(()),
                    Err(DisplayError::Fmt(e)) => Err(e),
                    Err(DisplayError::Decode(e)) => write!(f, "unknown({scheme}):error: {e:?}"),
                }
            }
        }
    }
}
