use super::*;
use alloc::borrow::Cow;
use thiserror::Error;

mod error;
mod parse;

pub use error::Error;

#[cfg(test)]
mod str_tests;

#[cfg(test)]
mod cbor_tests;

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
#[cfg_attr(feature = "serde", serde(into = "String"))]
#[cfg_attr(feature = "serde", serde(try_from = "Cow<str>"))]
pub enum Eid {
    #[default]
    Null,
    LocalNode {
        service_number: u32,
    },
    LegacyIpn {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Ipn {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Dtn {
        node_name: Box<str>,
        demux: Box<str>,
    },
    Unknown {
        scheme: u64,
        data: Box<[u8]>,
    },
}

impl TryFrom<Cow<'_, str>> for Eid {
    type Error = Error;

    fn try_from(value: Cow<'_, str>) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl hardy_cbor::encode::ToCbor for Eid {
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| match self {
            Eid::Null => {
                a.emit(&1);
                a.emit(&0);
            }
            Eid::Dtn { node_name, demux } => {
                a.emit(&1);
                a.emit(&format!("//{}/{}", urlencoding::encode(node_name), demux));
            }
            Eid::LegacyIpn {
                allocator_id,
                node_number,
                service_number,
            } => {
                a.emit(&2);
                a.emit_array(Some(2), |a| {
                    a.emit(&(((*allocator_id as u64) << 32) | *node_number as u64));
                    a.emit(service_number);
                });
            }
            Eid::Ipn {
                allocator_id: 0,
                node_number,
                service_number,
            } => {
                a.emit(&2);
                a.emit_array(Some(2), |a| {
                    a.emit(node_number);
                    a.emit(service_number);
                });
            }
            Eid::Ipn {
                allocator_id,
                node_number,
                service_number,
            } => {
                a.emit(&2);
                a.emit_array(Some(3), |a| {
                    a.emit(allocator_id);
                    a.emit(node_number);
                    a.emit(service_number);
                });
            }
            Eid::LocalNode { service_number } => {
                a.emit(&2);
                a.emit_array(Some(2), |a| {
                    a.emit(&u32::MAX);
                    a.emit(service_number);
                });
            }
            Eid::Unknown { scheme, data } => {
                a.emit(scheme);
                a.emit_raw_slice(data);
            }
        })
    }
}

#[derive(Error, Debug)]
enum DisplayError {
    #[error(transparent)]
    Decode(#[from] hardy_cbor::decode::Error),

    #[error(transparent)]
    Fmt(#[from] core::fmt::Error),
}

impl core::fmt::Display for Eid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Eid::Null => f.write_str("dtn:none"),
            Eid::LocalNode { service_number } => {
                write!(f, "ipn:!.{service_number}")
            }
            Eid::LegacyIpn {
                allocator_id: 0,
                node_number,
                service_number,
            }
            | Eid::Ipn {
                allocator_id: 0,
                node_number,
                service_number,
            } => write!(f, "ipn:{node_number}.{service_number}"),
            Eid::LegacyIpn {
                allocator_id,
                node_number,
                service_number,
            }
            | Eid::Ipn {
                allocator_id,
                node_number,
                service_number,
            } => write!(f, "ipn:{allocator_id}.{node_number}.{service_number}"),
            Eid::Dtn { node_name, demux } => {
                write!(f, "dtn://{}/{}", urlencoding::encode(node_name), demux)
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
