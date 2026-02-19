use hardy_async::sync::spin::Once;
use hardy_bpa::bpa::BpaRegistration;
use hardy_bpv7::eid::NodeId;
use std::{
    collections::HashMap,
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

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Invalid path '{0}'")]
    InvalidPath(String),

    #[error("Failed to create directory '{path}': {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },

    #[error("Failed to canonicalize path '{path}': {source}")]
    Canonicalize {
        path: String,
        source: std::io::Error,
    },

    #[error("Failed to get current working directory: {0}")]
    CurrentDir(std::io::Error),

    #[error("Failed to register CLA: {0}")]
    Registration(#[from] hardy_bpa::cla::Error),
}

fn canonicalize_path(cwd: &Path, path: &PathBuf) -> Result<String, Error> {
    let full_path = cwd.join(path);

    // Check everything is UTF-8
    if full_path.to_str().is_none() {
        return Err(Error::InvalidPath(format!("{}", full_path.display())));
    }

    // Ensure we have created the path
    std::fs::create_dir_all(&full_path).map_err(|e| Error::CreateDir {
        path: full_path.display().to_string(),
        source: e,
    })?;

    let canonical = full_path.canonicalize().map_err(|e| Error::Canonicalize {
        path: full_path.display().to_string(),
        source: e,
    })?;

    Ok(canonical.to_string_lossy().into_owned())
}

/// The main struct for the file-based Convergence Layer Adapter (CLA).
pub struct Cla {
    inboxes: HashMap<NodeId, String>,
    outbox: Option<String>,
    sink: Once<Arc<dyn hardy_bpa::cla::Sink>>,
    tasks: hardy_async::TaskPool,
}

impl Cla {
    /// Creates a new `Cla` instance.
    ///
    /// # Arguments
    ///
    /// * `config` - The configuration for this CLA.
    ///
    /// # Errors
    ///
    /// Returns an error if any configured paths are invalid or cannot be created.
    pub fn new(config: &Config) -> Result<Self, Error> {
        let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;

        // Canonicalize all peer inbox paths eagerly
        let mut inboxes = HashMap::new();
        for (eid, path) in &config.peers {
            let canonical = canonicalize_path(&cwd, path)?;
            inboxes.insert(eid.clone(), canonical);
        }

        // Canonicalize outbox path if configured
        let outbox = config
            .outbox
            .as_ref()
            .map(|path| canonicalize_path(&cwd, path))
            .transpose()?;

        Ok(Self {
            inboxes,
            outbox,
            sink: Once::new(),
            tasks: hardy_async::TaskPool::new(),
        })
    }

    /// Registers this CLA with the BPA.
    ///
    /// # Arguments
    ///
    /// * `bpa` - The BPA instance to register with.
    /// * `name` - The name to register this CLA under.
    pub async fn register(
        self: &Arc<Self>,
        bpa: &dyn BpaRegistration,
        name: String,
    ) -> Result<(), Error> {
        bpa.register_cla(name, None, self.clone(), None).await?;
        Ok(())
    }

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }
}
