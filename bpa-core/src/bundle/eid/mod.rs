use super::*;

mod error;
mod parse;

#[cfg(test)]
mod tests;

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
        parse::eid_from_cbor(data)
    }
}

impl std::str::FromStr for Eid {
    type Err = EidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse::eid_from_str(s)
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
