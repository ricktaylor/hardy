use core::cmp::Ordering;
use core::fmt::{Debug, Formatter, Result};
use core::hash::{Hash, Hasher};

use hardy_async::sync::spin::Mutex;
use hardy_bpv7::eid::NodeId;

use super::{Cla, ClaAddress};
use crate::{Arc, HashMap};

/// A registered CLA instance with its runtime state.
pub struct ClaEntry {
    pub(crate) cla: Arc<dyn Cla>,
    pub(crate) name: Arc<str>,
    pub(crate) peers: Mutex<HashMap<ClaAddress, Vec<NodeId>>>,
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

impl Hash for ClaEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl core::fmt::Display for ClaEntry {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}", self.name)
    }
}

impl Debug for ClaEntry {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("ClaEntry")
            .field("name", &self.name)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}
