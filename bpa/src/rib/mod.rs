use super::*;
use hardy_bpv7::eid::Eid;
use hardy_eid_pattern::EidPatternMap;
use std::{
    collections::{BinaryHeap, HashMap},
    sync::RwLock,
};

mod find;
mod local;
mod route;

pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<service_registry::Service>), // Deliver to local service
    Forward(
        Vec<u32>, // Available endpoints for forwarding
        bool,     // Should we reflect if forwarding fails
    ),
}

struct RibInner {
    locals: local::LocalInner,
    routes: EidPatternMap<route::Entry>,
    address_types: HashMap<cla::ClaAddressType, Arc<cla::registry::Cla>>,
}

pub struct Rib {
    inner: RwLock<RibInner>,
}

impl Rib {
    pub fn new(config: &config::Config) -> Self {
        Self {
            inner: RwLock::new(RibInner {
                locals: local::LocalInner::new(config),
                routes: EidPatternMap::new(),
                address_types: HashMap::new(),
            }),
        }
    }

    pub fn add_address_type(
        &self,
        address_type: cla::ClaAddressType,
        cla: Arc<cla::registry::Cla>,
    ) {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .address_types
            .insert(address_type, cla);
    }

    pub fn remove_address_type(&self, address_type: &cla::ClaAddressType) {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .address_types
            .remove(address_type);
    }
}
