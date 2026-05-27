use alloc::vec::Vec;

use flume::Sender;
use hardy_async::CancellationToken;
use hardy_bpv7::eid::NodeId;

use super::{Bytes, ClaAddress};

pub struct IngressBundle {
    pub data: Bytes,
    pub peer_node: Option<NodeId>,
    pub peer_addr: Option<ClaAddress>,
}

pub enum PeerOp {
    Add(ClaAddress, Vec<NodeId>),
    Remove(ClaAddress),
}

#[derive(Clone)]
pub struct ClaContext {
    ingress: Sender<IngressBundle>,
    peers: Sender<PeerOp>,
    shutdown: CancellationToken,
}

impl ClaContext {
    pub fn new(
        ingress: Sender<IngressBundle>,
        peers: Sender<PeerOp>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            ingress,
            peers,
            shutdown,
        }
    }

    pub async fn dispatch(
        &self,
        data: Bytes,
        peer_node: Option<NodeId>,
        peer_addr: Option<ClaAddress>,
    ) {
        let _ = self
            .ingress
            .send_async(IngressBundle {
                data,
                peer_node,
                peer_addr,
            })
            .await;
    }

    pub fn add_peer(&self, cla_addr: ClaAddress, node_ids: Vec<NodeId>) {
        let _ = self.peers.send(PeerOp::Add(cla_addr, node_ids));
    }

    pub fn remove_peer(&self, cla_addr: ClaAddress) {
        let _ = self.peers.send(PeerOp::Remove(cla_addr));
    }

    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.shutdown
    }

    pub fn is_connected(&self) -> bool {
        !self.ingress.is_disconnected() && !self.peers.is_disconnected()
    }
}
