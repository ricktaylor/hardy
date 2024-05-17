use super::*;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpnNodeId {
    allocator_id: u32,
    node_number: u32,
}

impl IpnNodeId {
    pub fn to_eid(&self, service_number: u32) -> Eid {
        Eid::Ipn3 {
            allocator_id: self.allocator_id,
            node_number: self.node_number,
            service_number,
        }
    }
}

impl std::fmt::Display for IpnNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.allocator_id != 0 {
            write!(f, "ipn:{}.{}.0", self.allocator_id, self.node_number)
        } else {
            write!(f, "ipn:{}.0", self.node_number)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtnNodeId {
    node_name: String,
}

impl DtnNodeId {
    pub fn to_eid(&self, demux: &str) -> Eid {
        Eid::Dtn {
            node_name: self.node_name.clone(),
            demux: demux.to_string(),
        }
    }
}

impl std::fmt::Display for DtnNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dtn://{}/", self.node_name)
    }
}

#[derive(Clone)]
pub struct NodeId {
    pub ipn: Option<IpnNodeId>,
    pub dtn: Option<DtnNodeId>,
}

impl NodeId {
    pub fn init(config: &config::Config) -> Self {
        // Load NodeId from config
        let node_id = init_from_value(
            config
                .get::<config::Value>("administrative_endpoint")
                .trace_expect(
                    "Missing or invalid 'administrative_endpoint' value in configuration",
                ),
        )
        .trace_expect("Invalid 'administrative_endpoint' value in configuration");

        match (&node_id.ipn, &node_id.dtn) {
            (None, None) => unreachable!(),
            (None, Some(node_id)) => info!("Administrative Endpoint: {node_id}"),
            (Some(node_id), None) => info!("Administrative Endpoint: {node_id}"),
            (Some(node_id1), Some(node_id2)) => {
                info!("Administrative endpoints: [{node_id1}, {node_id2}]")
            }
        }
        node_id
    }

    pub fn get_admin_endpoint(&self, destination: &Eid) -> Eid {
        match (&self.ipn, &self.dtn) {
            (None, Some(node_id)) => node_id.to_eid(""),
            (Some(node_id), None) => match destination {
                Eid::LocalNode { service_number: _ } => Eid::LocalNode { service_number: 0 },
                Eid::Ipn2 {
                    allocator_id: _,
                    node_number: _,
                    service_number: _,
                } => Eid::Ipn2 {
                    allocator_id: node_id.allocator_id,
                    node_number: node_id.node_number,
                    service_number: 0,
                },
                _ => node_id.to_eid(0),
            },
            (Some(ipn_node_id), Some(dtn_node_id)) => match destination {
                Eid::LocalNode { service_number: _ } => Eid::LocalNode { service_number: 0 },
                Eid::Ipn2 {
                    allocator_id: _,
                    node_number: _,
                    service_number: _,
                } => Eid::Ipn2 {
                    allocator_id: ipn_node_id.allocator_id,
                    node_number: ipn_node_id.node_number,
                    service_number: 0,
                },
                Eid::Dtn {
                    node_name: _,
                    demux: _,
                } => dtn_node_id.to_eid(""),
                _ => ipn_node_id.to_eid(0),
            },
            _ => unreachable!(),
        }
    }

    pub fn is_local_service(&self, eid: &Eid) -> bool {
        match eid {
            Eid::Null => false,
            Eid::LocalNode { service_number: _ } => true,
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number: _,
            }
            | Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number: _,
            } => match &self.ipn {
                Some(node_id) => {
                    node_id.allocator_id == *allocator_id && node_id.node_number == *node_number
                }
                _ => false,
            },
            Eid::Dtn {
                node_name,
                demux: _,
            } => match &self.dtn {
                Some(node_id) => node_id.node_name == *node_name,
                _ => false,
            },
        }
    }
}

#[derive(Error, Debug)]
enum Error {
    #[error("Value must be a string, table, or array")]
    InvalidValue,

    #[error("dtn URIs must be ASCII")]
    DtnNotASCII,

    #[error("dtn node-name is empty")]
    DtnNodeNameEmpty,

    #[error("dtn administrative endpoints must not have a demux part")]
    DtnHasDemux,

    #[error("Administrative endpoints must not be Null")]
    NotNone,

    #[error("More than 3 components in an ipn administrative endpoint")]
    IpnAdditionalItems,

    #[error("ipn administrative endpoints must have service number 0")]
    IpnNonZeroServiceNumber,

    #[error("Unsupported EID scheme {0}")]
    UnsupportedScheme(String),

    #[error("Multiple dtn administrative endpoints in configuration: {0}")]
    MultipleDtn(DtnNodeId),

    #[error("Multiple ipn administrative endpoints in configuration: {0}")]
    MultipleIpn(IpnNodeId),

    #[error("No administrative endpoints in configuration")]
    NoEndpoints,

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
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

fn init_from_value(v: config::Value) -> Result<NodeId, Error> {
    match v.kind {
        config::ValueKind::String(s) => init_from_string(s),
        config::ValueKind::Table(t) => init_from_table(t),
        config::ValueKind::Array(v) => init_from_array(v),
        _ => Err(Error::InvalidValue),
    }
}

fn init_from_dtn(s: &str) -> Result<NodeId, Error> {
    if !s.is_ascii() {
        Err(Error::DtnNotASCII)
    } else if let Some((s1, s2)) = s.split_once('/') {
        if s1.is_empty() {
            Err(Error::DtnNodeNameEmpty)
        } else if !s2.is_empty() {
            Err(Error::DtnHasDemux)
        } else {
            Ok(NodeId {
                dtn: Some(DtnNodeId {
                    node_name: s1.to_string(),
                }),
                ipn: None,
            })
        }
    } else {
        Ok(NodeId {
            dtn: Some(DtnNodeId {
                node_name: s.to_string(),
            }),
            ipn: None,
        })
    }
}

fn init_from_ipn(s: &str) -> Result<NodeId, Error> {
    let parts = s.split('.').collect::<Vec<&str>>();
    if parts.len() == 1 {
        let node_number = parts[0].parse::<u32>().map_field_err("Node Number")?;
        if node_number == 0 {
            Err(Error::NotNone)
        } else {
            Ok(NodeId {
                ipn: Some(IpnNodeId {
                    allocator_id: 0,
                    node_number,
                }),
                dtn: None,
            })
        }
    } else if parts.len() == 2 {
        let node_number = parts[0].parse::<u32>().map_field_err("Node Number")?;
        let service_number = parts[1].parse::<u32>().map_field_err("Service Number")?;
        if service_number != 0 {
            Err(Error::IpnNonZeroServiceNumber)
        } else if node_number == 0 {
            Err(Error::NotNone)
        } else {
            Ok(NodeId {
                ipn: Some(IpnNodeId {
                    allocator_id: 0,
                    node_number,
                }),
                dtn: None,
            })
        }
    } else if parts.len() == 3 {
        let allocator_id = parts[0]
            .parse::<u32>()
            .map_field_err("Allocator Identifier")?;
        let node_number = parts[1].parse::<u32>().map_field_err("Node Number")?;
        let service_number = parts[2].parse::<u32>().map_field_err("Service Number")?;
        if service_number != 0 {
            Err(Error::IpnNonZeroServiceNumber)
        } else if allocator_id == 0 && node_number == 0 {
            Err(Error::NotNone)
        } else {
            Ok(NodeId {
                ipn: Some(IpnNodeId {
                    allocator_id,
                    node_number,
                }),
                dtn: None,
            })
        }
    } else {
        Err(Error::IpnAdditionalItems)
    }
}

fn init_from_string(s: String) -> Result<NodeId, Error> {
    if let Some(s) = s.strip_prefix("dtn://") {
        init_from_dtn(s)
    } else if let Some(s) = s.strip_prefix("ipn:") {
        init_from_ipn(s)
    } else if s == "dtn:none" {
        Err(Error::NotNone)
    } else if let Some((schema, _)) = s.split_once(':') {
        Err(Error::UnsupportedScheme(schema.to_string()))
    } else {
        Err(Error::UnsupportedScheme(s.to_string()))
    }
}

fn init_from_table(t: HashMap<String, config::Value>) -> Result<NodeId, Error> {
    let mut node_id = NodeId {
        ipn: None,
        dtn: None,
    };
    for (k, v) in t {
        let n = match k.as_str() {
            "dtn" => {
                let s = v.into_string().map_field_err("dtn node id")?;
                if s == "none" {
                    Err(Error::NotNone)
                } else {
                    init_from_dtn(&s)
                }
            }
            "ipn" => match v.kind {
                config::ValueKind::I64(v) if v < (2 ^ 32) - 1 => Ok(NodeId {
                    dtn: None,
                    ipn: Some(IpnNodeId {
                        allocator_id: 0,
                        node_number: v as u32,
                    }),
                }),
                config::ValueKind::U64(v) if v < (2 ^ 32) - 1 => Ok(NodeId {
                    dtn: None,
                    ipn: Some(IpnNodeId {
                        allocator_id: 0,
                        node_number: v as u32,
                    }),
                }),
                config::ValueKind::I128(v) if v < (2 ^ 32) - 1 => Ok(NodeId {
                    dtn: None,
                    ipn: Some(IpnNodeId {
                        allocator_id: 0,
                        node_number: v as u32,
                    }),
                }),
                config::ValueKind::U128(v) if v < (2 ^ 32) - 1 => Ok(NodeId {
                    dtn: None,
                    ipn: Some(IpnNodeId {
                        allocator_id: 0,
                        node_number: v as u32,
                    }),
                }),
                _ => {
                    let s = v.into_string().map_field_err("ipn node id")?;
                    init_from_ipn(&s)
                }
            },
            _ => return Err(Error::UnsupportedScheme(k)),
        }?;

        match (&node_id.dtn, n.dtn) {
            (None, Some(dtn_node_id)) => node_id.dtn = Some(dtn_node_id),
            (Some(dtn_node_id1), Some(dtn_node_id2)) => {
                if *dtn_node_id1 == dtn_node_id2 {
                    info!("Duplicate \"administrative_endpoint\" in configuration: {dtn_node_id1}")
                } else {
                    return Err(Error::MultipleDtn(dtn_node_id2));
                }
            }
            _ => {}
        }
        match (&node_id.ipn, n.ipn) {
            (None, Some(ipn_node_id)) => node_id.ipn = Some(ipn_node_id),
            (Some(ipn_node_id1), Some(ipn_node_id2)) => {
                if *ipn_node_id1 == ipn_node_id2 {
                    info!("Duplicate \"administrative_endpoint\" in configuration: {ipn_node_id1}")
                } else {
                    return Err(Error::MultipleIpn(ipn_node_id2));
                }
            }
            _ => {}
        }
    }

    // Check we have at least one endpoint!
    if node_id.ipn.is_none() && node_id.dtn.is_none() {
        Err(Error::NoEndpoints)
    } else {
        Ok(node_id)
    }
}

fn init_from_array(t: Vec<config::Value>) -> Result<NodeId, Error> {
    let mut node_id = NodeId {
        ipn: None,
        dtn: None,
    };
    for v in t {
        let n = init_from_value(v)?;
        match (&node_id.dtn, n.dtn) {
            (None, Some(dtn_node_id)) => node_id.dtn = Some(dtn_node_id),
            (Some(dtn_node_id1), Some(dtn_node_id2)) => {
                if *dtn_node_id1 == dtn_node_id2 {
                    info!("Duplicate \"administrative_endpoint\" in configuration: {dtn_node_id1}")
                } else {
                    return Err(Error::MultipleDtn(dtn_node_id2));
                }
            }
            _ => {}
        }
        match (&node_id.ipn, n.ipn) {
            (None, Some(ipn_node_id)) => node_id.ipn = Some(ipn_node_id),
            (Some(ipn_node_id1), Some(ipn_node_id2)) => {
                if *ipn_node_id1 == ipn_node_id2 {
                    info!("Duplicate \"administrative_endpoint\" in configuration: {ipn_node_id1}")
                } else {
                    return Err(Error::MultipleIpn(ipn_node_id2));
                }
            }
            _ => {}
        }
    }

    // Check we have at least one endpoint!
    if node_id.ipn.is_none() && node_id.dtn.is_none() {
        Err(Error::NoEndpoints)
    } else {
        Ok(node_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_config<T: Into<config::Value>>(v: T) -> config::Value {
        config::Config::builder()
            .set_default("administrative_endpoint", v)
            .unwrap()
            .build()
            .unwrap()
            .get::<config::Value>("administrative_endpoint")
            .unwrap()
    }

    #[test]
    fn test() {
        let n = init_from_value(fake_config("ipn:1.0")).unwrap();
        assert!(n.dtn.is_none());
        assert!(n.ipn.map_or(false, |node_id| match node_id {
            IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            } => true,
            _ => false,
        }));

        let n = init_from_value(fake_config("ipn:2.1.0")).unwrap();
        assert!(n.dtn.is_none());
        assert!(n.ipn.map_or(false, |node_id| match node_id {
            IpnNodeId {
                allocator_id: 2,
                node_number: 1,
            } => true,
            _ => false,
        }));

        let n = init_from_value(fake_config("dtn://node-name/")).unwrap();
        assert!(n.ipn.is_none());
        assert!(n
            .dtn
            .map_or(false, |node_id| node_id.node_name == "node-name"));

        /*#administrative_endpoint = { "ipn": N, "dtn": "node-name" }
        #administrative_endpoint = { "ipn": "[A.]N", "dtn": "node-name" }
        #administrative_endpoint = [ "ipn:[A.]N.0", "dtn://node-name/"]*/
    }
}
