use core::cmp::Ordering;
use core::fmt::{Debug, Display, Formatter, Result};
use core::hash::{Hash, Hasher};

use hardy_async::sync::spin::Mutex;
use hardy_bpv7::eid::NodeId;

use super::{Cla, ClaAddress};
use crate::{Arc, HashMap};

/// A registered CLA instance with its runtime state.
pub struct Adapter {
    pub(crate) cla: Arc<dyn Cla>,
    pub(crate) name: Arc<str>,
    pub(crate) peers: Mutex<HashMap<ClaAddress, Vec<NodeId>>>,
}

impl PartialEq for Adapter {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Adapter {}

impl PartialOrd for Adapter {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Adapter {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name.cmp(&other.name)
    }
}

impl Hash for Adapter {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

impl Display for Adapter {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}", self.name)
    }
}

impl Debug for Adapter {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("Adapter")
            .field("name", &self.name)
            .field("peers", &self.peers)
            .finish_non_exhaustive()
    }
}
