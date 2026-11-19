/*!
File-based Convergence Layer Adapter (CLA) for the Bundle Protocol Agent.

This crate provides a CLA that uses the local filesystem as a transport mechanism
for DTN bundles. Bundles arriving in a watched "outbox" directory are dispatched
to the BPA, while bundles forwarded by the BPA are written as files into
per-peer "inbox" directories. This is useful for testing, bridging air-gapped
networks via removable media, or integrating with external tools that produce
or consume raw bundle files.
*/

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use core::num::NonZeroU64;

use hardy_async::TaskPool;
// `cla` is this crate's own module holding the trait impl, so the BPA's `cla`
// items are imported by name, with its `Error` aliased to avoid the clash.
use hardy_bpa::cla::{Context, Error as ClaError, Factory, Request, Sink, factory_fn};
use hardy_bpv7::eid::NodeId;

mod cla;
mod watcher;

/// Configuration for the file-based Convergence Layer Adapter (CLA).
///
/// Both fields default to empty, no outbox and no peers, so the CLA is
/// inert until configured.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    /// The directory to watch for new files to be sent as bundles.
    ///
    /// Each file in this directory is a complete bundle, dispatched to the
    /// BPA and then deleted.
    ///
    /// Default: `None` (no outbox; inbound file ingestion is disabled).
    pub outbox: Option<PathBuf>,
    /// A map of peer Endpoint IDs (EIDs) to their corresponding inbox directories.
    ///
    /// A bundle forwarded to a peer is written as a file in that peer's
    /// directory, under a filename derived from the bundle's source EID,
    /// timestamp and optional fragment offset.
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
    Registration(#[from] ClaError),
}

fn canonicalize_path(cwd: &Path, path: &PathBuf) -> Result<String, Error> {
    let full_path = cwd.join(path);

    if full_path.to_str().is_none() {
        return Err(Error::InvalidPath(format!("{}", full_path.display())));
    }

    fs::create_dir_all(&full_path).map_err(|e| Error::CreateDir {
        path: full_path.display().to_string(),
        source: e,
    })?;

    let canonical = full_path.canonicalize().map_err(|e| Error::Canonicalize {
        path: full_path.display().to_string(),
        source: e,
    })?;

    Ok(canonical.to_string_lossy().into_owned())
}

/// A convergence layer that carries bundles as files on the local filesystem.
pub struct Cla {
    inboxes: HashMap<NodeId, String>,
    outbox: Option<String>,
    sink: Arc<dyn Sink>,
    max_bundle_size: Option<NonZeroU64>,
    tasks: TaskPool,
}

impl Cla {
    /// A factory for a file CLA, resolving every configured directory now.
    ///
    /// Outbox and inbox paths are canonicalized against the current working
    /// directory, and created if they do not exist, so that a missing or
    /// relative path is a configuration error rather than a surprise at the
    /// first file operation.
    ///
    /// # Errors
    ///
    /// Returns an error if a configured path is not UTF-8, or cannot be
    /// created or canonicalized.
    pub fn factory(config: &Config) -> Result<impl Factory<Component = Self> + use<>, Error> {
        let cwd = env::current_dir().map_err(Error::CurrentDir)?;

        let mut inboxes = HashMap::new();
        for (eid, path) in &config.peers {
            let canonical = canonicalize_path(&cwd, path)?;
            inboxes.insert(eid.clone(), canonical);
        }

        let outbox = config
            .outbox
            .as_ref()
            .map(|path| canonicalize_path(&cwd, path))
            .transpose()?;

        Ok(factory_fn(Request::default(), move |cx: Context| {
            Ok(Arc::new(Self {
                inboxes,
                outbox,
                sink: cx.sink,
                max_bundle_size: cx.terms.max_bundle_size,
                tasks: TaskPool::new(),
            }))
        }))
    }

    /// Unregisters this CLA from the BPA.
    pub async fn unregister(&self) {
        self.sink.unregister().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A relative path is joined onto the supplied cwd and the directory chain
    // is created.
    #[test]
    fn test_canonicalize_relative_path_joins_cwd() {
        let cwd = tempfile::tempdir().unwrap();

        let result = canonicalize_path(cwd.path(), &PathBuf::from("nested/dir")).unwrap();

        let expected = cwd.path().join("nested/dir");
        assert!(expected.is_dir(), "directory chain should be created");
        assert_eq!(PathBuf::from(result), expected.canonicalize().unwrap());
    }

    // An absolute path ignores the cwd entirely (Path::join semantics); nothing
    // is created under the cwd.
    #[test]
    fn test_canonicalize_absolute_path_ignores_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let target = other.path().join("inbox");

        let result = canonicalize_path(cwd.path(), &target).unwrap();

        assert!(target.is_dir());
        assert_eq!(PathBuf::from(result), target.canonicalize().unwrap());
        assert_eq!(
            fs::read_dir(cwd.path()).unwrap().count(),
            0,
            "an absolute path must not be prefixed with the cwd"
        );
    }

    // A regular file already occupying the target path surfaces as CreateDir,
    // not Canonicalize.
    #[test]
    fn test_canonicalize_existing_file_is_create_dir_error() {
        let cwd = tempfile::tempdir().unwrap();
        fs::write(cwd.path().join("occupied"), b"not a directory").unwrap();

        let err = canonicalize_path(cwd.path(), &PathBuf::from("occupied")).unwrap_err();
        assert!(matches!(err, Error::CreateDir { .. }), "got {err:?}");
    }

    // A non-UTF-8 path component is rejected up front as InvalidPath.
    #[cfg(unix)]
    #[test]
    fn test_canonicalize_non_utf8_is_invalid_path() {
        use std::os::unix::ffi::OsStringExt;

        let cwd = tempfile::tempdir().unwrap();
        let component = std::ffi::OsString::from_vec(vec![0x66, 0xff, 0xfe]);

        let err = canonicalize_path(cwd.path(), &PathBuf::from(component)).unwrap_err();
        assert!(matches!(err, Error::InvalidPath(_)), "got {err:?}");
    }
}
