use super::*;
use std::collections::HashMap;

#[derive(Clone)]
pub struct NodeId {
    pub ipn: Option<bundle::Eid>,
    pub dtn: Option<bundle::Eid>,
}

impl NodeId {
    pub fn init(config: &config::Config) -> Result<Self, anyhow::Error> {
        // Load NodeId from config
        let node_id = init_from_value(
            config
                .get::<config::Value>("administrative_endpoint")
                .map_err(|e| {
                    anyhow!(
                        "Missing \"administrative_endpoint\" from configuration: {}",
                        e
                    )
                })?,
        )?;

        match (&node_id.ipn, &node_id.dtn) {
            (None, None) => unreachable!(),
            (None, Some(eid)) | (Some(eid), None) => log::info!("Administrative Endpoint: {eid}"),
            (Some(eid1), Some(eid2)) => log::info!("Administrative Endpoints: [{eid1}, {eid2}]"),
        }
        Ok(node_id)
    }

    pub fn get_admin_endpoint(&self, destination: &bundle::Eid) -> bundle::Eid {
        match destination {
            bundle::Eid::LocalNode { service_number: _ } => {
                bundle::Eid::LocalNode { service_number: 0 }
            }
            bundle::Eid::Ipn2 {
                allocator_id: _,
                node_number: _,
                service_number: _,
            } => match (&self.ipn, &self.dtn) {
                (None, Some(eid)) => eid.clone(),
                (
                    Some(bundle::Eid::Ipn3 {
                        allocator_id,
                        node_number,
                        service_number,
                    }),
                    _,
                ) => bundle::Eid::Ipn2 {
                    allocator_id: *allocator_id,
                    node_number: *node_number,
                    service_number: *service_number,
                },
                _ => unreachable!(),
            },
            bundle::Eid::Null
            | bundle::Eid::Ipn3 {
                allocator_id: _,
                node_number: _,
                service_number: _,
            } => match (&self.ipn, &self.dtn) {
                (None, Some(eid)) | (Some(eid), _) => eid.clone(),
                _ => unreachable!(),
            },
            bundle::Eid::Dtn {
                node_name: _,
                demux: _,
            } => match (&self.ipn, &self.dtn) {
                (_, Some(eid)) | (Some(eid), None) => eid.clone(),
                _ => unreachable!(),
            },
        }
    }

    pub fn is_local_service(&self, eid: &bundle::Eid) -> bool {
        match eid {
            bundle::Eid::Null => false,
            bundle::Eid::LocalNode { service_number: _ } => true,
            bundle::Eid::Ipn2 {
                allocator_id: a,
                node_number: n,
                service_number: _,
            }
            | bundle::Eid::Ipn3 {
                allocator_id: a,
                node_number: n,
                service_number: _,
            } => match &self.ipn {
                Some(bundle::Eid::Ipn3 {
                    allocator_id,
                    node_number,
                    service_number: _,
                }) => allocator_id == a && node_number == n,
                _ => false,
            },
            bundle::Eid::Dtn {
                node_name: name,
                demux: _,
            } => match &self.dtn {
                Some(bundle::Eid::Dtn {
                    node_name,
                    demux: _,
                }) => node_name == name,
                _ => false,
            },
        }
    }
}

fn init_from_value(v: config::Value) -> Result<NodeId, anyhow::Error> {
    match v.kind {
        config::ValueKind::String(s) => init_from_string(s),
        config::ValueKind::Table(t) => init_from_table(t),
        config::ValueKind::Array(v) => init_from_array(v),
        v => Err(anyhow!(
            "Invalid \"administrative_endpoint\" in configuration: {}",
            v
        )),
    }
}

fn init_from_string(s: String) -> Result<NodeId, anyhow::Error> {
    let eid = s.parse::<bundle::Eid>()?;
    match eid {
        bundle::Eid::Ipn3 {
            allocator_id: _,
            node_number: _,
            service_number: 0,
        } => Ok(NodeId {
            ipn: Some(eid),
            dtn: None,
        }),
        bundle::Eid::Dtn {
            node_name: _,
            ref demux,
        } if demux.is_empty() => Ok(NodeId {
            dtn: Some(eid),
            ipn: None,
        }),
        eid => Err(anyhow!(
            "Invalid \"administrative_endpoint\" in configuration: {}",
            eid
        )),
    }
}

fn init_from_table(t: HashMap<String, config::Value>) -> Result<NodeId, anyhow::Error> {
    let mut node_id = NodeId {
        ipn: None,
        dtn: None,
    };
    for (k, v) in t {
        let n = match k.as_str() {
            "dtn" => {
                let s = v.into_string().map_err(|e| e.extend_with_key(&k))?;
                if s.split_once('/').is_some() {
                    Err(anyhow!(
                        "Invalid \"administrative_endpoint\" dtn node-name {k} in configuration"
                    ))
                } else {
                    Ok(NodeId {
                        dtn: Some(bundle::Eid::Dtn {
                            node_name: s,
                            demux: String::new(),
                        }),
                        ipn: None,
                    })
                }
            }
            "ipn" => match v.kind {
                config::ValueKind::U64(v) if v < (2 ^ 32) - 1 => Ok(NodeId {
                    dtn: None,
                    ipn: Some(bundle::Eid::Ipn3 {
                        allocator_id: 0,
                        node_number: v as u32,
                        service_number: 0,
                    }),
                }),
                config::ValueKind::U128(v) if v < (2 ^ 32) - 1 => Ok(NodeId {
                    dtn: None,
                    ipn: Some(bundle::Eid::Ipn3 {
                        allocator_id: 0,
                        node_number: v as u32,
                        service_number: 0,
                    }),
                }),
                config::ValueKind::String(s) => {
                    let mut parts = s.split('.');
                    if let Some(value) = parts.next() {
                        let v1 = value.parse::<u32>()?;
                        if let Some(value) = &parts.next() {
                            let v2 = value.parse::<u32>()?;
                            if parts.next().is_some() {
                                Err(anyhow!("Invalid \"administrative_endpoint\" ipn FQNN {s} in configuration"))
                            } else {
                                Ok(NodeId {
                                    dtn: None,
                                    ipn: Some(bundle::Eid::Ipn3 {
                                        allocator_id: v1,
                                        node_number: v2,
                                        service_number: 0,
                                    }),
                                })
                            }
                        } else {
                            Ok(NodeId {
                                dtn: None,
                                ipn: Some(bundle::Eid::Ipn3 {
                                    allocator_id: 0,
                                    node_number: v1,
                                    service_number: 0,
                                }),
                            })
                        }
                    } else {
                        let v1 = s.parse::<u32>()?;
                        Ok(NodeId {
                            dtn: None,
                            ipn: Some(bundle::Eid::Ipn3 {
                                allocator_id: 0,
                                node_number: v1,
                                service_number: 0,
                            }),
                        })
                    }
                }
                _ => Err(anyhow!(
                    "Invalid \"administrative_endpoint\" ipn FQNN {k} in configuration"
                )),
            },
            _ => {
                return Err(anyhow!(
                    "Unsupported \"administrative_endpoint\" EID scheme {k} in configuration"
                ))
            }
        }?;
        match (&node_id.dtn, n.dtn) {
            (None, Some(eid)) => node_id.dtn = Some(eid),
            (Some(eid1), Some(eid2)) => {
                if *eid1 == eid2 {
                    log::info!(
                        "Duplicate \"administrative_endpoint\" in configuration: {}",
                        eid1
                    )
                } else {
                    return Err(anyhow!(
                        "Multiple \"administrative_endpoint\" dtn entries in configuration: {}",
                        eid2
                    ));
                }
            }
            _ => {}
        }
        match (&node_id.ipn, n.ipn) {
            (None, Some(eid)) => node_id.ipn = Some(eid),
            (Some(eid1), Some(eid2)) => {
                if *eid1 == eid2 {
                    log::info!(
                        "Duplicate \"administrative_endpoint\" in configuration: {}",
                        eid1
                    )
                } else {
                    return Err(anyhow!(
                        "Multiple \"administrative_endpoint\" ipn entries in configuration: {}",
                        eid2
                    ));
                }
            }
            _ => {}
        }
    }

    // Check we have at least one endpoint!
    if node_id.ipn.is_none() && node_id.dtn.is_none() {
        return Err(anyhow!(
            "No valid \"administrative_endpoint\" entries in configuration!"
        ));
    }
    Ok(node_id)
}

fn init_from_array(t: Vec<config::Value>) -> Result<NodeId, anyhow::Error> {
    let mut node_id = NodeId {
        ipn: None,
        dtn: None,
    };
    for v in t {
        let n = init_from_value(v)?;
        match (&node_id.dtn, n.dtn) {
            (None, Some(eid)) => node_id.dtn = Some(eid),
            (Some(eid1), Some(eid2)) => {
                if *eid1 == eid2 {
                    log::info!(
                        "Duplicate \"administrative_endpoint\" in configuration: {}",
                        eid1
                    )
                } else {
                    return Err(anyhow!(
                        "Multiple \"administrative_endpoint\" dtn entries in configuration: {}",
                        eid2
                    ));
                }
            }
            _ => {}
        }
        match (&node_id.ipn, n.ipn) {
            (None, Some(eid)) => node_id.ipn = Some(eid),
            (Some(eid1), Some(eid2)) => {
                if *eid1 == eid2 {
                    log::info!(
                        "Duplicate \"administrative_endpoint\" in configuration: {}",
                        eid1
                    )
                } else {
                    return Err(anyhow!(
                        "Multiple \"administrative_endpoint\" ipn entries in configuration: {}",
                        eid2
                    ));
                }
            }
            _ => {}
        }
    }

    // Check we have at least one endpoint!
    if node_id.ipn.is_none() && node_id.dtn.is_none() {
        return Err(anyhow!(
            "No valid \"administrative_endpoint\" entries in configuration!"
        ));
    }
    Ok(node_id)
}

#[cfg(test)]
mod tests {
    use super::{bundle, NodeId};

    fn make_config<T: Into<config::Value>>(v: T) -> config::Config {
        config::Config::builder()
            .set_default("administrative_endpoint", v)
            .unwrap()
            .build()
            .unwrap()
    }

    #[test]
    fn test() {
        let n = NodeId::init(&make_config("ipn:1.0")).unwrap();
        assert!(n.dtn.is_none());
        assert!(n.ipn.map_or(false, |eid| match eid {
            bundle::Eid::Ipn3 {
                allocator_id: 0,
                node_number: 1,
                service_number: 0,
            } => true,
            _ => false,
        }));

        let n = NodeId::init(&make_config("ipn:2.1.0")).unwrap();
        assert!(n.dtn.is_none());
        assert!(n.ipn.map_or(false, |eid| match eid {
            bundle::Eid::Ipn3 {
                allocator_id: 2,
                node_number: 1,
                service_number: 0,
            } => true,
            _ => false,
        }));

        let n = NodeId::init(&make_config("dtn://node-name/")).unwrap();
        assert!(n.dtn.is_none());
        assert!(n.ipn.map_or(false, |eid| match eid {
            bundle::Eid::Dtn { node_name, demux } => node_name == "node-name" && demux.is_empty(),
            _ => false,
        }));

        /*#administrative_endpoint = { "ipn": N, "dtn": "node-name" }
        #administrative_endpoint = { "ipn": "[A.]N", "dtn": "node-name" }
        #administrative_endpoint = [ "ipn:[A.]N.0", "dtn://node-name/"]*/
    }
}
