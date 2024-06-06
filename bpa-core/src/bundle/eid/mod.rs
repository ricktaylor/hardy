use super::*;

mod error;
mod parse;

use error::CaptureFieldErr;
use parse::*;

pub use error::EidError;

#[derive(Default, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum Eid {
    #[default]
    Null,
    LocalNode {
        service_number: u32,
    },
    Ipn2 {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Ipn3 {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Dtn {
        node_name: String,
        demux: Vec<String>,
    },
}

impl cbor::encode::ToCbor for &Eid {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        match self {
            Eid::Null => encoder.emit_array(Some(2), |a| {
                a.emit(1);
                a.emit(0)
            }),
            Eid::Dtn { node_name, demux } => encoder.emit_array(Some(2), |a| {
                a.emit(1);
                a.emit(format!(
                    "//{}/{}",
                    urlencoding::encode(node_name),
                    demux
                        .iter()
                        .map(|s| urlencoding::encode(s))
                        .collect::<Vec<std::borrow::Cow<str>>>()
                        .join("/")
                ))
            }),
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            } => encoder.emit_array(Some(2), |a| {
                a.emit(2);
                a.emit_array(Some(2), |a| {
                    a.emit((*allocator_id as u64) << 32 | *node_number as u64);
                    a.emit(*service_number);
                })
            }),
            Eid::Ipn3 {
                allocator_id: 0,
                node_number,
                service_number,
            } => encoder.emit_array(Some(2), |a| {
                a.emit(2);
                a.emit_array(Some(2), |a| {
                    a.emit(*node_number);
                    a.emit(*service_number)
                })
            }),
            Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => encoder.emit_array(Some(2), |a| {
                a.emit(2);
                a.emit_array(Some(3), |a| {
                    a.emit(*allocator_id);
                    a.emit(*node_number);
                    a.emit(*service_number)
                })
            }),
            Eid::LocalNode { service_number } => encoder.emit_array(Some(2), |a| {
                a.emit(2);
                a.emit_array(Some(2), |a| {
                    a.emit((2u64 ^ 32) - 1);
                    a.emit(*service_number)
                })
            }),
        }
    }
}

impl cbor::decode::FromCbor for Eid {
    type Error = error::EidError;

    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), Self::Error> {
        cbor::decode::parse_array(data, |a, tags| {
            if a.count().is_none() {
                trace!("Parsing EID array of indefinite length")
            }
            let schema = a.parse::<u64>().map_field_err("Scheme")?;
            let (eid, _) = a
                .parse_value(|value, _, tags2| {
                    if !tags2.is_empty() {
                        trace!("Parsing EID value with tags");
                    }
                    match (schema, value) {
                        (1, value) => parse_dtn_eid(value),
                        (2, cbor::decode::Value::Array(a)) => parse_ipn_eid(a),
                        (2, value) => Err(cbor::decode::Error::IncorrectType(
                            "Array".to_string(),
                            value.type_name(),
                        )
                        .into()),
                        _ => Err(EidError::UnsupportedScheme(schema.to_string())),
                    }
                })
                .map_field_err("Scheme-specific part")?;
            if a.end()?.is_none() {
                Err(EidError::AdditionalItems)
            } else {
                Ok((eid, tags.to_vec()))
            }
        })
        .map(|((eid, tags), len)| (eid, len, tags))
    }
}

impl std::str::FromStr for Eid {
    type Err = EidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(s) = s.strip_prefix("dtn://") {
            parse_dtn_parts(s)
        } else if let Some(s) = s.strip_prefix("ipn:") {
            let parts = s.split('.').collect::<Vec<&str>>();
            if parts.len() == 2 {
                Ok(Self::Ipn3 {
                    allocator_id: 0,
                    node_number: parts[0].parse::<u32>().map_field_err("Node Number")?,
                    service_number: parts[1].parse::<u32>().map_field_err("Service Number")?,
                })
            } else if parts.len() == 3 {
                Ok(Self::Ipn3 {
                    allocator_id: parts[0]
                        .parse::<u32>()
                        .map_field_err("Allocator Identifier")?,
                    node_number: parts[1].parse::<u32>().map_field_err("Node Number")?,
                    service_number: parts[2].parse::<u32>().map_field_err("Service Number")?,
                })
            } else {
                Err(EidError::IpnAdditionalItems)
            }
        } else if s == "dtn:none" {
            Ok(Eid::Null)
        } else if let Some((schema, _)) = s.split_once(':') {
            Err(EidError::UnsupportedScheme(schema.to_string()))
        } else {
            Err(EidError::UnsupportedScheme(s.to_string()))
        }
    }
}

impl std::fmt::Debug for Eid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Eid::Ipn2 {
                allocator_id: 0,
                node_number,
                service_number,
            } => write!(f, "ipn(2):{node_number}.{service_number}"),
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            } => write!(f, "ipn(2):{allocator_id}.{node_number}.{service_number}"),
            _ => <Self as std::fmt::Display>::fmt(self, f),
        }
    }
}

impl std::fmt::Display for Eid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Eid::Null => f.write_str("ipn:0.0"),
            Eid::LocalNode { service_number } => {
                write!(f, "ipn:!.{service_number}")
            }
            Eid::Ipn2 {
                allocator_id: 0,
                node_number,
                service_number,
            }
            | Eid::Ipn3 {
                allocator_id: 0,
                node_number,
                service_number,
            } => write!(f, "ipn:{node_number}.{service_number}"),
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            }
            | Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => write!(f, "ipn:{allocator_id}.{node_number}.{service_number}"),
            Eid::Dtn { node_name, demux } => write!(
                f,
                "dtn://{}/{}",
                urlencoding::encode(node_name),
                demux
                    .iter()
                    .map(|s| urlencoding::encode(s))
                    .collect::<Vec<std::borrow::Cow<str>>>()
                    .join("/")
            ),
        }
    }
}
