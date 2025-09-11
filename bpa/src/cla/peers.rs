use super::*;
use hardy_bpv7::eid::Eid;
use registry::Cla;
use std::{
    collections::HashMap,
    sync::{Mutex, RwLock},
};

struct Peer {
    eid: Eid,
    cla: Arc<Cla>,
    addr: ClaAddress,
}

#[derive(Default)]
struct PeerTableInner {
    peers: HashMap<u32, Peer>,
    next: u32,
}

impl PeerTableInner {
    fn insert(&mut self, cla: Arc<Cla>, eid: Eid, addr: ClaAddress) -> u32 {
        // TODO: Poll the store

        todo!()
    }

    fn remove(&mut self, peer_id: u32) -> bool {
        // TODO: Kill the store poller

        todo!()
    }
}

pub struct PeerTable {
    inner: RwLock<PeerTableInner>,
}

impl PeerTable {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(PeerTableInner::default()),
        }
    }

    pub fn insert(&self, cla: Arc<Cla>, eid: Eid, addr: ClaAddress) -> u32 {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .insert(cla, eid, addr)
    }

    pub fn remove(&self, peer_id: u32) -> bool {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .remove(peer_id)
    }
}
