use hardy_bpv7::eid::NodeId;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use trace_err::*;
use tracing::{debug, error, info, warn};

mod cla;
mod watcher;

/// Configuration for the file-based Convergence Layer Adapter (CLA).
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    /// The directory to watch for new files to be sent as bundles.
    /// Each file in this directory is treated as a complete bundle and will be
    /// dispatched to the BPA. After dispatch, the file is deleted.
    pub outbox: Option<PathBuf>,
    /// A map of peer Endpoint IDs (EIDs) to their corresponding inbox directories.
    /// When a bundle is to be forwarded to a peer, it will be written as a file
    /// in the directory associated with that peer's EID.
    pub peers: HashMap<NodeId, PathBuf>,
}

struct ClaInner {
    sink: Arc<dyn hardy_bpa::cla::Sink>,
    inboxes: HashSet<String>,
}

/// The main struct for the file-based Convergence Layer Adapter (CLA).
pub struct Cla {
    _name: String,
    config: Config,
    inner: std::sync::OnceLock<ClaInner>,
    tasks: hardy_async::TaskPool,
}

impl Cla {
    /// Creates a new `Cla` instance.
    ///
    /// # Arguments
    ///
    /// * `name` - A name for this CLA instance.
    /// * `config` - The configuration for this CLA.
    pub fn new(name: String, config: Config) -> Self {
        Self {
            config,
            _name: name,
            inner: std::sync::OnceLock::new(),
            tasks: hardy_async::TaskPool::new(),
        }
    }

    /// Unregisters the CLA instance from the BPA.
    pub async fn unregister(&self) -> bool {
        let Some(inner) = self.inner.get() else {
            return false;
        };
        inner.sink.unregister().await;
        true
    }
}
