use super::*;

// One registered CLA: the component, what it declared, and the peers it has
// published.
pub struct Cla {
    pub cla: Arc<dyn cla::Cla>,
    pub policy: Arc<dyn policy::FlowControllerFactory>,
    // Snapshotted from the factory's `ClaInit` declarations at registration.
    pub address_type: Option<ClaAddressType>,
    pub lane_count: Option<NonZeroU32>,

    pub(super) name: Arc<str>,
    // Cancelled at unregistration; every in-flight stream of this
    // registration races it and fails immediately.
    pub(super) cancel: hardy_async::CancellationToken,
    // sync::spin::Mutex for O(1) peer HashMap operations
    // Key: ClaAddress (primary key for a link-layer adjacency)
    // Value: (known EIDs for the peer, peer_id in PeerTable)
    // An empty EID vec means a Neighbour (EID not yet known; no RIB entry installed)
    pub(super) peers: hardy_async::sync::spin::Mutex<HashMap<ClaAddress, (Vec<NodeId>, u32)>>,
}

impl PartialEq for Cla {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Cla {}

impl PartialOrd for Cla {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Cla {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.name.cmp(&other.name)
    }
}

impl core::hash::Hash for Cla {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl core::fmt::Debug for Cla {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cla")
            .field("name", &self.name)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}

impl Cla {
    // Whether `peer_id` is one of this CLA's peers — the ownership check for
    // deferred transfer outcomes.
    pub fn owns_peer(&self, peer_id: u32) -> bool {
        self.peers.lock().values().any(|(_, id)| *id == peer_id)
    }
}
