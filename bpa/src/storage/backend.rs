//! Storage backend contract.
//!
//! Everything in this module is the surface that a storage backend
//! implements against. Backend authors should be able to read this file
//! end-to-end without needing the rest of the storage subsystem.
//!
//! Re-exported from `crate::storage`, so external crates may equally
//! `use hardy_bpa::storage::{MetadataStorage, BundleStorage}` or
//! `use hardy_bpa::storage::backend::*`.

use hardy_async::async_trait;
use hardy_bpv7::{bundle::Id, eid::Eid};
use time::OffsetDateTime;

use crate::{
    Arc, Bytes,
    bundle::{Bundle, BundleMetadata, BundleStatus},
    stream::Sender,
};

/// Boxed error type used by storage trait methods.
pub type Error = Box<dyn core::error::Error + Send + Sync>;
/// Result alias for storage operations.
pub type Result<T> = core::result::Result<T, Error>;

/// The `MetadataStorage` trait defines the interface for storing and managing bundle metadata.
///
/// This trait provides a set of asynchronous methods for interacting with the metadata storage,
/// including inserting, retrieving, and updating bundle metadata. It also includes methods for
/// more complex operations such as recovering unconfirmed bundles, polling for bundles in
/// various states, and managing the lifecycle of bundles within the storage system.
///
/// Implementers of this trait are expected to provide a thread-safe and efficient implementation
/// of these methods.
///
/// # Streaming Results
///
/// Polling methods (`poll_*`, `remove_unconfirmed`) deliver results to the caller via a
/// [`Sender<Bundle>`] sink rather than returning them as a collection. This decouples the
/// trait from any specific channel implementation: the BPA passes a `hardy_async::channel::Sender`
/// directly (it implements [`Sender`] via a blanket impl), localdisk-storage builds an adapter
/// over its internal flume channel, and tests use a `Vec`-collecting mock — all implementing the
/// same [`Sender`] trait. Implementors should stop iterating when `stream.send` returns
/// `Err(SendError(_))` — the consumer has gone away.
#[async_trait]
pub trait MetadataStorage: Send + Sync {
    /// Retrieves the metadata for the bundle with the given `bundle_id`, or
    /// `None` if no entry exists.
    async fn get(&self, bundle_id: &Id) -> Result<Option<Bundle>>;

    /// Inserts a new bundle's metadata, returning whether it was newly
    /// inserted (`false` if an entry already exists).
    async fn insert(&self, bundle: &Bundle) -> Result<bool>;

    /// Replaces an existing bundle's metadata.
    async fn replace(&self, bundle: &Bundle) -> Result<()>;

    /// Updates only the typed status columns for an existing bundle's metadata.
    ///
    /// Cheaper than `replace` because the bundle blob is not written. Use this
    /// for pure state-machine transitions where no other metadata has changed.
    async fn update_status(&self, bundle: &Bundle) -> Result<()>;

    /// Removes any metadata for the given `bundle_id` and leaves a "tombstone".
    ///
    /// A tombstone marks the bundle as deleted, preventing it from being
    /// re-inserted or processed further. Does not error if the bundle does
    /// not exist.
    async fn tombstone(&self, bundle_id: &Id) -> Result<()>;

    /// Begins the startup recovery protocol by marking all existing metadata
    /// entries as unconfirmed. The BPA then calls `confirm_exists()` for each
    /// bundle it finds in the bundle store, and finally calls
    /// `remove_unconfirmed()` to clean up any orphaned metadata.
    ///
    /// Non-persistent backends should treat this as a no-op.
    async fn start_recovery(&self);

    /// Part of the startup recovery protocol. Called once per bundle after
    /// `start_recovery()` as the BPA walks the bundle store and finds data on
    /// disk. Confirms that the metadata entry for this bundle is still wanted,
    /// and returns its metadata so the BPA can resume processing.
    ///
    /// For persistent backends (e.g. SQLite), this removes the bundle from the
    /// "unconfirmed" set populated by `start_recovery()`. Any entries still in
    /// that set when `remove_unconfirmed()` is called are metadata records
    /// whose corresponding bundle data was lost.
    ///
    /// Non-persistent backends (e.g. in-memory) have nothing to recover, so
    /// this returns `Ok(None)`.
    async fn confirm_exists(&self, bundle_id: &Id) -> Result<Option<BundleMetadata>>;

    /// Final step of the startup recovery protocol. Removes all metadata
    /// entries that were not confirmed via `confirm_exists()` since the last
    /// `start_recovery()` call, and pushes the removed bundles to `stream` so
    /// the BPA can perform any necessary cleanup (e.g. deleting bundle data).
    /// Stops early if `stream.send` returns `Err(SendError(_))`.
    ///
    /// Non-persistent backends should treat this as a no-op.
    async fn remove_unconfirmed(&self, stream: &dyn Sender<Bundle>) -> Result<()>;

    /// Resets all bundles with status `BundleStatus::ForwardPending { peer, .. }`
    /// to `Waiting`, so the dispatcher re-evaluates their forwarding decision.
    /// Returns the number of bundles reset.
    async fn reset_peer_queue(&self, peer: u32) -> Result<u64>;

    /// Pushes the next `limit` bundles, excluding status `BundleStatus::New`
    /// and ordered by expiry, to `stream`. Stops early if `stream.send`
    /// returns `Err(SendError(_))`.
    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>, limit: usize) -> Result<()>;

    /// Pushes all `BundleStatus::Waiting` bundles, snapshotted at the time of
    /// the call and ordered by received time, to `stream`. Stops early if
    /// `stream.send` returns `Err(SendError(_))`.
    async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> Result<()>;

    /// Pushes all `BundleStatus::WaitingForService` bundles for the given
    /// `source`, ordered by received time, to `stream`. Stops early if
    /// `stream.send` returns `Err(SendError(_))`.
    async fn poll_service_waiting(&self, source: Eid, stream: &dyn Sender<Bundle>) -> Result<()>;

    /// Pushes all bundles matching the given `BundleStatus::AduFragment`
    /// `status`, preferably ordered by fragment offset, to `stream`. Stops
    /// early if `stream.send` returns `Err(SendError(_))`.
    async fn poll_adu_fragments(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
    ) -> Result<()>;

    /// Pushes the next `limit` bundles in the given `status`, ordered by
    /// received time, to `stream`. Stops early if `stream.send` returns
    /// `Err(SendError(_))`.
    async fn poll_pending(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
        limit: usize,
    ) -> Result<()>;
}

/// A recovered bundle entry: `(storage_name, creation_time)`.
pub type RecoveryResponse = (Arc<str>, OffsetDateTime);

/// The `BundleStorage` trait defines the interface for storing and managing the binary data of bundles.
///
/// This trait provides a set of asynchronous methods for interacting with the bundle storage,
/// including saving, loading, and deleting bundle data. It also includes a method for recovering
/// bundles from the storage.
///
/// Implementers of this trait are expected to provide a thread-safe and efficient implementation
/// of these methods.
///
/// # Streaming Results
///
/// `recover` delivers entries to the caller via a [`Sender<RecoveryResponse>`] sink rather
/// than returning a collection. See the [`MetadataStorage`] trait docs for the rationale.
#[async_trait]
pub trait BundleStorage: Send + Sync {
    /// Recovers bundles from the bundle storage, pushing each to `stream`.
    /// Stops early if `stream.send` returns `Err(SendError(_))`.
    async fn recover(&self, stream: &dyn Sender<RecoveryResponse>) -> Result<()>;

    /// Loads the bundle stored under `storage_name`, or `None` if absent.
    ///
    /// Loading is non-destructive: repeated loads of the same name return
    /// the same data until [`delete`](BundleStorage::delete) or
    /// [`replace`](BundleStorage::replace). The BPA re-loads on every
    /// forwarding retry.
    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>>;

    /// Saves bundle `data`, returning the generated storage name.
    async fn save(&self, data: Bytes) -> Result<Arc<str>>;

    /// Overwrites existing bundle data at the given storage name.
    ///
    /// The implementation must ensure atomicity: readers see either the
    /// old data or the new data, never a partial write.
    async fn replace(&self, storage_name: &str, data: Bytes) -> Result<()>;

    /// Deletes the bundle stored under `storage_name`.
    async fn delete(&self, storage_name: &str) -> Result<()>;
}
