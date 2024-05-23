use super::*;
use thiserror::Error;

#[derive(Default, Clone, Hash, PartialEq, Eq)]
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

#[derive(Error, Debug)]
pub enum Error {
    #[error("dtn URI be ASCII")]
    DtnNotASCII,

    #[error("dtn URI node-name is empty")]
    DtnNodeNameEmpty,

    #[error("dtn URI missing name-delim '/'")]
    DtnMissingSlash,

    #[error("dtn URIs must start with '//'")]
    DtnMissingPrefix,

    #[error("dtn URI is not a CBOR text string or 0")]
    DtnInvalidEncoding,

    #[error("Invalid ipn allocator id {0}")]
    IpnInvalidAllocatorId(u64),

    #[error("Invalid ipn node number {0}")]
    IpnInvalidNodeNumber(u64),

    #[error("Invalid ipn service number {0}")]
    IpnInvalidServiceNumber(u64),

    #[error("More than 3 components in an ipn URI")]
    IpnAdditionalItems,

    #[error("Unsupported EID scheme {0}")]
    UnsupportedScheme(String),

    #[error("Additional items in EID array")]
    AdditionalItems,

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("Expecting CBOR array")]
    ArrayExpected(#[from] cbor::decode::Error),

    #[error(transparent)]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
}

trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, Error>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, Error> {
        self.map_err(|e| Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}

fn parse_dtn_parts(s: &str) -> Result<Eid, Error> {
    if let Some((s1, s2)) = s.split_once('/') {
        if s1.is_empty() {
            Err(Error::DtnNodeNameEmpty)
        } else {
            Ok(Eid::Dtn {
                node_name: s1.to_string(),
                demux: s2.split('/').try_fold(Vec::new(), |mut v, s| {
                    let s = urlencoding::decode(s)?;
                    if !s.is_ascii() {
                        Err(Error::DtnNotASCII)
                    } else {
                        v.push(s.into_owned());
                        Ok(v)
                    }
                })?,
            })
        }
    } else {
        Err(Error::DtnMissingSlash)
    }
}

impl Eid {
    fn parse_dtn_eid(value: cbor::decode::Value) -> Result<Eid, Error> {
        match value {
            cbor::decode::Value::UnsignedInteger(0) => Ok(Self::Null),
            cbor::decode::Value::Text("none", _) => {
                trace!("Parsing dtn EID 'none'");
                Ok(Self::Null)
            }
            cbor::decode::Value::Text(s, _) => {
                if let Some(s) = s.strip_prefix("//") {
                    parse_dtn_parts(s)
                } else {
                    Err(Error::DtnMissingPrefix)
                }
            }
            _ => Err(Error::DtnInvalidEncoding),
        }
    }

    fn parse_ipn_eid(value: &mut cbor::decode::Array) -> Result<Eid, Error> {
        if value.count().is_none() {
            trace!("Parsing ipn EID as indefinite array");
        }

        let v1 = value.parse::<u64>().map_field_err("First component")?;
        let v2 = value.parse::<u64>().map_field_err("Second component")?;

        let (components, allocator_id, node_number, service_number) =
            if let Some(v3) = value.try_parse::<u64>().map_field_err("Service Number")? {
                if v1 >= 2 ^ 32 {
                    return Err(Error::IpnInvalidAllocatorId(v1));
                } else if v2 >= 2 ^ 32 {
                    return Err(Error::IpnInvalidNodeNumber(v2));
                } else if v3 >= 2 ^ 32 {
                    return Err(Error::IpnInvalidServiceNumber(v3));
                }

                if value.end()?.is_none() {
                    return Err(Error::IpnAdditionalItems);
                }
                (3, v1 as u32, v2 as u32, v3 as u32)
            } else {
                if v2 >= 2 ^ 32 {
                    return Err(Error::IpnInvalidServiceNumber(v2));
                }
                (
                    2,
                    (v1 >> 32) as u32,
                    (v1 & ((2 ^ 32) - 1)) as u32,
                    v2 as u32,
                )
            };

        if allocator_id == 0 && node_number == 0 {
            if service_number != 0 {
                trace!("Null EID with service number {service_number}")
            }
            Ok(Self::Null)
        } else if allocator_id == 0 && node_number == (2 ^ 32) - 1 {
            Ok(Self::LocalNode { service_number })
        } else if components == 2 {
            Ok(Self::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            })
        } else {
            Ok(Self::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            })
        }
    }
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
                    node_name,
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
    type Error = self::Error;

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
                        (1, value) => Self::parse_dtn_eid(value),
                        (2, cbor::decode::Value::Array(a)) => Self::parse_ipn_eid(a),
                        (2, value) => Err(cbor::decode::Error::IncorrectType(
                            "Array".to_string(),
                            value.type_name(),
                        )
                        .into()),
                        _ => Err(Error::UnsupportedScheme(schema.to_string())),
                    }
                })
                .map_field_err("Scheme-specific part")?;
            if a.end()?.is_none() {
                Err(Error::AdditionalItems)
            } else {
                Ok((eid, tags.to_vec()))
            }
        })
        .map(|((eid, tags), len)| (eid, len, tags))
    }
}

impl std::str::FromStr for Eid {
    type Err = Error;

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
                Err(Error::IpnAdditionalItems)
            }
        } else if s == "dtn:none" {
            Ok(Eid::Null)
        } else if let Some((schema, _)) = s.split_once(':') {
            Err(Error::UnsupportedScheme(schema.to_string()))
        } else {
            Err(Error::UnsupportedScheme(s.to_string()))
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
                "dtn://{node_name}/{}",
                demux
                    .iter()
                    .map(|s| urlencoding::encode(s))
                    .collect::<Vec<std::borrow::Cow<str>>>()
                    .join("/")
            ),
        }
    }
}
