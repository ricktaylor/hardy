/*!
File-based Convergence Layer Adapter (CLA) for the Bundle Protocol Agent.

This crate provides a CLA that uses the local filesystem as a transport mechanism
for DTN bundles. Bundles arriving in a watched "outbox" directory are dispatched
to the BPA, while bundles forwarded by the BPA are written as files into
per-peer "inbox" directories. This is useful for testing, bridging air-gapped
networks via removable media, or integrating with external tools that produce
or consume raw bundle files.
*/

use hardy_async::sync::spin::Once;
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
///
/// All fields have sensible defaults (via `Default`): no outbox and no peers,
/// meaning the CLA is inert until explicitly configured.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    /// The directory to watch for new files to be sent as bundles.
    ///
    /// Each file in this directory is treated as a complete bundle and will be
    /// dispatched to the BPA. After successful dispatch the file is deleted.
    ///
    /// Default: `None` (no outbox; inbound file ingestion is disabled).
    pub outbox: Option<PathBuf>,
    /// A map of peer Endpoint IDs (EIDs) to their corresponding inbox directories.
    ///
    /// When a bundle is to be forwarded to a peer, it will be written as a file
    /// in the directory associated with that peer's EID. The filename is derived
    /// from the bundle's source EID, timestamp, and optional fragment offset.
    ///
    /// Default: empty map (no peers configured).
    pub peers: HashMap<NodeId, PathBuf>,
}

/// Errors that can occur during CLA initialisation or registration.
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// A configured path is not valid UTF-8.
    #[error("Invalid path '{0}'")]
    InvalidPath(String),

    /// A required directory could not be created.
    #[error("Failed to create directory '{path}': {source}")]
    CreateDir {
        /// The path that could not be created.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A path could not be canonicalized to an absolute form.
    #[error("Failed to canonicalize path '{path}': {source}")]
    Canonicalize {
        /// The path that could not be canonicalized.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The current working directory could not be determined.
    #[error("Failed to get current working directory: {0}")]
    CurrentDir(std::io::Error),

    /// CLA registration with the BPA failed.
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

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }
}
