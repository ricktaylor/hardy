use super::*;
use bpv7::Eid;
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
    pub fn to_eid(&self, demux: &str) -> Result<Eid, super::Error> {
        // Roundtrip via String for PctEncoding safety
        let mut s = self.to_string();
        s.push_str(demux);
        s.parse::<Eid>().map_err(Into::into)
    }
}

impl std::fmt::Display for DtnNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Eid::Dtn {
            node_name: self.node_name.clone(),
            demux: Vec::new(),
        }
        .fmt(f)
    }
}

#[derive(Clone)]
pub struct AdminEndpoints {
    pub ipn: Option<IpnNodeId>,
    pub dtn: Option<DtnNodeId>,
}

impl AdminEndpoints {
    pub fn init(config: &config::Config) -> Self {
        // Load NodeId from config
        let admin_endpoints = init_from_value(
            config
                .get::<config::Value>("administrative_endpoint")
                .trace_expect(
                    "Missing or invalid 'administrative_endpoint' value in configuration",
                ),
        )
        .trace_expect("Invalid 'administrative_endpoint' value in configuration");

        match (&admin_endpoints.ipn, &admin_endpoints.dtn) {
            (None, None) => unreachable!(),
            (None, Some(node_id)) => info!("Administrative Endpoint: {node_id}"),
            (Some(node_id), None) => info!("Administrative Endpoint: {node_id}"),
            (Some(node_id1), Some(node_id2)) => {
                info!("Administrative endpoints: [{node_id1}, {node_id2}]")
            }
        }
        admin_endpoints
    }

    pub fn get_admin_endpoint(&self, destination: &Eid) -> Eid {
        match (&self.ipn, &self.dtn) {
            (None, Some(node_id)) => Eid::Dtn {
                node_name: node_id.node_name.clone(),
                demux: Vec::new(),
            },
            (Some(node_id), None) => match destination {
                Eid::LocalNode { .. } => Eid::LocalNode { service_number: 0 },
                Eid::Ipn2 { .. } => Eid::Ipn2 {
                    allocator_id: node_id.allocator_id,
                    node_number: node_id.node_number,
                    service_number: 0,
                },
                _ => node_id.to_eid(0),
            },
            (Some(ipn_node_id), Some(dtn_node_id)) => match destination {
                Eid::LocalNode { .. } => Eid::LocalNode { service_number: 0 },
                Eid::Ipn2 { .. } => Eid::Ipn2 {
                    allocator_id: ipn_node_id.allocator_id,
                    node_number: ipn_node_id.node_number,
                    service_number: 0,
                },
                Eid::Dtn { .. } => Eid::Dtn {
                    node_name: dtn_node_id.node_name.clone(),
                    demux: Vec::new(),
                },
                _ => ipn_node_id.to_eid(0),
            },
            _ => unreachable!(),
        }
    }

    pub fn is_local_service(&self, eid: &Eid) -> bool {
        match eid {
            Eid::LocalNode { .. } => true,
            Eid::Ipn2 {
                allocator_id,
                node_number,
                ..
            }
            | Eid::Ipn3 {
                allocator_id,
                node_number,
                ..
            } => match &self.ipn {
                Some(node_id) => {
                    node_id.allocator_id == *allocator_id && node_id.node_number == *node_number
                }
                _ => false,
            },
            Eid::Dtn { node_name, .. } => match &self.dtn {
                Some(node_id) => node_id.node_name == *node_name,
                _ => false,
            },
            _ => false,
        }
    }

    pub fn is_admin_endpoint(&self, eid: &Eid) -> bool {
        match eid {
            Eid::LocalNode { service_number } => *service_number == 0,
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            }
            | Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => match &self.ipn {
                Some(node_id) => {
                    node_id.allocator_id == *allocator_id
                        && node_id.node_number == *node_number
                        && *service_number == 0
                }
                _ => false,
            },
            Eid::Dtn { node_name, demux } => match &self.dtn {
                Some(node_id) => node_id.node_name == *node_name && demux.is_empty(),
                _ => false,
            },
            _ => false,
        }
    }
}

#[derive(Error, Debug)]
enum Error {
    #[error("Value must be a string or array of strings")]
    InvalidValue,

    #[error("dtn administrative endpoints must not have a demux part")]
    DtnHasDemux,

    #[error("Administrative endpoints must not be Null")]
    NotNone,

    #[error("Administrative endpoints must not be LocalNode")]
    NotLocalNode,

    #[error("ipn administrative endpoints must have service number 0")]
    IpnNonZeroServiceNumber,

    #[error("Multiple dtn administrative endpoints in configuration: {0}")]
    MultipleDtn(DtnNodeId),

    #[error("Multiple ipn administrative endpoints in configuration: {0}")]
    MultipleIpn(IpnNodeId),

    #[error("No administrative endpoints in configuration")]
    NoEndpoints,

    #[error(transparent)]
    Parser(#[from] bpv7::EidError),

    #[error(transparent)]
    Config(#[from] config::ConfigError),
}

fn init_from_value(v: config::Value) -> Result<AdminEndpoints, Error> {
    match v.kind {
        config::ValueKind::String(s) => init_from_string(s),
        config::ValueKind::Array(v) => init_from_array(v),
        _ => Err(Error::InvalidValue),
    }
}

fn init_from_string(s: String) -> Result<AdminEndpoints, Error> {
    match s.parse::<bpv7::Eid>()? {
        Eid::Null => Err(Error::NotNone),
        Eid::LocalNode { .. } => Err(Error::NotLocalNode),
        Eid::Ipn2 {
            allocator_id,
            node_number,
            service_number,
        }
        | Eid::Ipn3 {
            allocator_id,
            node_number,
            service_number,
        } => {
            if service_number != 0 {
                Err(Error::IpnNonZeroServiceNumber)
            } else {
                Ok(AdminEndpoints {
                    ipn: Some(IpnNodeId {
                        allocator_id,
                        node_number,
                    }),
                    dtn: None,
                })
            }
        }
        Eid::Dtn { node_name, demux } => {
            if !demux.is_empty() && (demux.len() > 1 || !demux[0].is_empty()) {
                Err(Error::DtnHasDemux)
            } else {
                Ok(AdminEndpoints {
                    dtn: Some(DtnNodeId { node_name }),
                    ipn: None,
                })
            }
        }
        _ => unreachable!(),
    }
}

fn init_from_array(t: Vec<config::Value>) -> Result<AdminEndpoints, Error> {
    let mut admin_endpoints = AdminEndpoints {
        ipn: None,
        dtn: None,
    };
    for v in t {
        let n = init_from_value(v)?;
        match (&admin_endpoints.dtn, n.dtn) {
            (None, Some(dtn_node_id)) => admin_endpoints.dtn = Some(dtn_node_id),
            (Some(dtn_node_id1), Some(dtn_node_id2)) => {
                if *dtn_node_id1 == dtn_node_id2 {
                    info!("Duplicate \"administrative_endpoint\" in configuration: {dtn_node_id1}")
                } else {
                    return Err(Error::MultipleDtn(dtn_node_id2));
                }
            }
            _ => {}
        }
        match (&admin_endpoints.ipn, n.ipn) {
            (None, Some(ipn_node_id)) => admin_endpoints.ipn = Some(ipn_node_id),
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
    if admin_endpoints.ipn.is_none() && admin_endpoints.dtn.is_none() {
        Err(Error::NoEndpoints)
    } else {
        Ok(admin_endpoints)
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

    fn ipn_test(config: &str, expected: IpnNodeId) {
        let a = init_from_value(fake_config(config)).unwrap();
        assert!(a.dtn.is_none());
        assert!(a.ipn.map_or(false, |node_id| node_id == expected));
    }

    fn dtn_test(config: &str, expected: &str) {
        let a = init_from_value(fake_config(config)).unwrap();
        assert!(a.ipn.is_none());
        assert!(a.dtn.map_or(false, |node_id| node_id.node_name == expected));
    }

    #[test]
    fn test() {
        ipn_test(
            "ipn:1.0",
            IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            },
        );

        ipn_test(
            "ipn:2.1.0",
            IpnNodeId {
                allocator_id: 2,
                node_number: 1,
            },
        );

        dtn_test("dtn://node-name/", "node-name");

        /*
        #administrative_endpoint = [ "ipn:[A.]N.0", "dtn://node-name/"]*/
    }
}
