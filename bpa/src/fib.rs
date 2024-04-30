use super::*;
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

impl Entry {
    pub fn from(eid: &bundle::Eid) -> Self {
        match eid {
            bundle::Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            } => Self::Ipn2 {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: *service_number,
            },
            bundle::Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => Self::Ipn3 {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: *service_number,
            },
            bundle::Eid::Dtn { node_name, demux } => Self::Dtn {
                node_name: node_name.clone(),
                demux: demux.clone(),
            },
            bundle::Eid::Null | bundle::Eid::LocalNode { service_number:_ } => panic!("Invalid FIB entry EID: {eid}"),
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
            .log_expect("Invalid 'forwarding' in configuration")
        {
            Some(Self {
                actions: Default::default(),
            })
        } else {
            None
        }
    }

    pub fn add_action(&self, from: Entry, action: Action) -> Option<Action> {
        self.actions
            .write()
            .log_expect("Failed to write-lock actions mutex")
            .insert(from, action)
    }
}
