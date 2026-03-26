use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // --- Bundle state ---
    #[error("bundle {id} already exists")]
    DuplicateBundle { id: hardy_bpv7::bundle::Id },

    #[error("bundle {id} not found")]
    BundleNotFound { id: hardy_bpv7::bundle::Id },

    #[error("metadata store returned bundle {found} when {expected} was requested")]
    BundleMismatch {
        expected: hardy_bpv7::bundle::Id,
        found: hardy_bpv7::bundle::Id,
    },

    // --- Bundle data operations ---
    #[error("could not write bundle data: {0}")]
    CouldNotWriteBundleData(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not read bundle data: {0}")]
    CouldNotReadBundleData(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not delete bundle data: {0}")]
    CouldNotDeleteBundleData(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not edit bundle: {0}")]
    CouldNotEditBundle(#[from] hardy_bpv7::editor::Error),

    // --- Metadata operations ---
    #[error("could not insert bundle metadata: {0}")]
    CouldNotInsertMetadata(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not read bundle metadata: {0}")]
    CouldNotReadMetadata(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not replace bundle metadata: {0}")]
    CouldNotReplaceMetadata(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not persist bundle status: {0}")]
    CouldNotUpdateStatus(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not write bundle tombstone: {0}")]
    CouldNotWriteTombstone(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not confirm bundle existence: {0}")]
    CouldNotConfirmBundle(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not poll bundle metadata: {0}")]
    CouldNotPollMetadata(#[source] Box<dyn core::error::Error + Send + Sync>),

    #[error("could not reset peer forwarding queue: {0}")]
    CouldNotResetPeerQueue(#[source] Box<dyn core::error::Error + Send + Sync>),

    // --- Storage consistency (discovered during recovery) ---
    /// Bundle data was expected in storage but is no longer present.
    #[error("bundle data not found in storage")]
    BundleDataMissing,

    /// Bundle data was found in storage but a copy already exists elsewhere.
    #[error("duplicate bundle data found in storage")]
    DuplicateBundleData,

    /// Bundle data exists in storage but cannot be parsed as a valid bundle.
    #[error("corrupt bundle data in storage")]
    CorruptBundleData,

    // --- Infrastructure ---
    /// Sending end of a storage channel was closed by the receiver.
    #[error("storage channel closed")]
    ChannelClosed,
}

impl Error {
    /// Increments the recovery metric counter associated with this error variant, if any.
    /// Call this in the storage recovery loop to keep metric names co-located with errors.
    pub(crate) fn increment_metric(&self) {
        match self {
            Error::BundleDataMissing => metrics::counter!("restart_lost_bundles").increment(1),
            Error::DuplicateBundleData => {
                metrics::counter!("restart_duplicate_bundles").increment(1)
            }
            Error::CorruptBundleData => metrics::counter!("restart_junk_bundles").increment(1),
            _ => {}
        }
    }
}

impl<'a> From<(hardy_bpv7::editor::Editor<'a>, hardy_bpv7::editor::Error)> for Error {
    fn from((_, e): (hardy_bpv7::editor::Editor<'a>, hardy_bpv7::editor::Error)) -> Self {
        Error::CouldNotEditBundle(e)
    }
}

impl From<flume::SendError<crate::bundle::Bundle>> for Error {
    fn from(_: flume::SendError<crate::bundle::Bundle>) -> Self {
        Error::ChannelClosed
    }
}

impl From<flume::SendError<super::RecoveryResponse>> for Error {
    fn from(_: flume::SendError<super::RecoveryResponse>) -> Self {
        Error::ChannelClosed
    }
}
