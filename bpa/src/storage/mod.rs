//! BPA storage subsystem.
//!
//! The backend contract — what an external storage backend implements
//! against — lives in [`backend`] and is re-exported here for convenience.
//! Everything else in this module (the in-process `Store`, the dispatcher
//! channel, the reaper, the recover/reassembly helpers, and the
//! `ChannelStreamIn` adapter) is BPA-internal infrastructure.

use hardy_async::async_trait;

pub mod backend;

mod bundle_mem;
mod cached;
mod metadata_mem;
mod reaper;

pub(crate) mod adu_reassembly;
pub(crate) mod channel;
pub(crate) mod recover;
pub(crate) mod store;

/// Receiver handle for bundles drained from a hybrid storage channel.
/// `recv()` returns `Err(RecvError::Disconnected)` after the buffer drains
/// once the channel has been closed.
pub(crate) type Receiver = hardy_async::closeable::Receiver<crate::bundle::Bundle>;

/// Adapter that exposes a [`hardy_async::channel::Sender<T>`] as a
/// [`StreamIn<T>`]. Used internally by the BPA at call sites that create
/// a channel and pass the sender into a storage trait method.
pub(crate) struct ChannelStreamIn<T>(pub hardy_async::channel::Sender<T>);

impl<T> ChannelStreamIn<T> {
    /// Convenience constructor that creates a bounded
    /// [`hardy_async::channel`] and wraps the sender in a
    /// `ChannelStreamIn`, returning it alongside the receiver.
    pub fn bounded(capacity: usize) -> (Self, hardy_async::channel::Receiver<T>) {
        let (tx, rx) = hardy_async::channel::bounded(capacity);
        (Self(tx), rx)
    }
}

#[async_trait]
impl<T: Send + 'static> StreamIn<T> for ChannelStreamIn<T> {
    async fn send(&self, item: T) -> core::result::Result<(), StreamClosed<T>> {
        self.0
            .send(item)
            .await
            .map_err(|hardy_async::channel::SendError(item)| StreamClosed(item))
    }
}

// Re-exports

/// In-memory [`BundleStorage`] backend, suitable for testing and ephemeral deployments.
pub use bundle_mem::{BundleMemStorage, Config as BundleMemStorageConfig};
/// In-memory [`MetadataStorage`] backend, suitable for testing and ephemeral deployments.
pub use metadata_mem::{Config as MetadataMemStorageConfig, MetadataMemStorage};

pub use cached::{CachedBundleStorage, DEFAULT_LRU_CAPACITY, DEFAULT_MAX_CACHED_BUNDLE_SIZE};

pub use backend::*;
