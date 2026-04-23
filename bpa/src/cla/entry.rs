use core::cmp::Ordering;

use hardy_async::sync::spin::Mutex;
use hardy_bpv7::eid::NodeId;

use super::{Cla, ClaAddress};
use crate::policy::EgressPolicy;
use crate::{Arc, HashMap};

/// A registered CLA instance with its runtime state.
pub struct ClaEntry {
    pub(super) cla: Arc<dyn Cla>,
    pub(super) policy: Arc<dyn EgressPolicy>,
    pub(super) name: String,
    pub(super) peers: Mutex<HashMap<ClaAddress, (Vec<NodeId>, u32)>>,
}

impl PartialEq for ClaEntry {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for ClaEntry {}

impl PartialOrd for ClaEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ClaEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

impl core::hash::Hash for ClaEntry {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl core::fmt::Debug for ClaEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClaEntry")
            .field("name", &self.name)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}
