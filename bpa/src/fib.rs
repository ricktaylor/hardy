use super::*;
use base64::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone, Hash, PartialEq, Eq)]
pub enum Entry {
    Cla {
        protocol: String,
        address: Vec<u8>,
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
        demux: String,
    },
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Entry::Cla { protocol, address } => {
                write!(f, "{protocol}: {}", BASE64_STANDARD_NO_PAD.encode(address))
            }
            Entry::Ipn2 {
                allocator_id: 0,
                node_number,
                service_number,
            }
            | Entry::Ipn3 {
                allocator_id: 0,
                node_number,
                service_number,
            } => write!(f, "ipn:{node_number}.{service_number}"),
            Entry::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            }
            | Entry::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => write!(f, "ipn:{allocator_id}.{node_number}.{service_number}"),
            Entry::Dtn { node_name, demux } => write!(f, "dtn://{node_name}/{demux}"),
        }
    }
}

impl From<&ingress::ClaSource> for Entry {
    fn from(value: &ingress::ClaSource) -> Self {
        Entry::Cla {
            protocol: value.protocol.clone(),
            address: value.address.clone(),
        }
    }
}

impl TryFrom<&bundle::Eid> for Entry {
    type Error = anyhow::Error;

    fn try_from(value: &bundle::Eid) -> Result<Self, Self::Error> {
        match value {
            bundle::Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            } => Ok(Self::Ipn2 {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: *service_number,
            }),
            bundle::Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => Ok(Self::Ipn3 {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: *service_number,
            }),
            bundle::Eid::Dtn { node_name, demux } => Ok(Self::Dtn {
                node_name: node_name.clone(),
                demux: demux.clone(),
            }),
            bundle::Eid::Null | bundle::Eid::LocalNode { service_number: _ } => {
                Err(anyhow!("Invalid FIB entry EID: {value}"))
            }
        }
    }
}

pub enum Action {
    Drop,
    Wait,
    Forward(Entry),
}

#[derive(Clone)]
pub struct Fib {
    actions: Arc<RwLock<HashMap<Entry, Action>>>,
}

impl Fib {
    pub fn new(config: &config::Config) -> Option<Self> {
        if settings::get_with_default(config, "forwarding", true)
            .log_expect("Invalid 'forwarding' value in configuration")
        {
            Some(Self {
                actions: Default::default(),
            })
        } else {
            None
        }
    }

    pub fn add_action(&self, to: Entry, action: Action) -> Result<Option<Action>, anyhow::Error> {
        let mut map = self
            .actions
            .write()
            .log_expect("Failed to write-lock actions mutex");

        let Action::Forward(next) = &action else {
            // Not recursive, just add
            return Ok(map.insert(to, action));
        };

        if to == *next {
            return Err(anyhow!("Recursive FIB entry {}", to));
        }

        add_action_recursive(&mut map, to, action)
    }
}

fn add_action_recursive(
    map: &mut HashMap<Entry, Action>,
    to: Entry,
    action: Action,
) -> Result<Option<Action>, anyhow::Error> {
    todo!()
}
