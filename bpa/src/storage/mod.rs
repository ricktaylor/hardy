use flume::Sender;
use hardy_async::async_trait;
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;
use time::OffsetDateTime;

use crate::bundle::{Bundle, BundleMetadata, BundleStatus};
use crate::{Arc, Bytes};

/// Boxed error type used by storage trait methods.
pub type Error = Box<dyn core::error::Error + Send + Sync>;
/// Result alias for storage operations.
pub type Result<T> = core::result::Result<T, Error>;

mod bundle_mem;
mod cached;
mod metadata_mem;
mod reaper;
mod store;

pub(crate) mod adu_reassembly;
pub(crate) mod channel;
pub(crate) mod recover;

/// In-memory [`BundleStorage`] backend, suitable for testing and ephemeral deployments.
pub use bundle_mem::{BundleMemStorage, Config as BundleMemStorageConfig};
pub use cached::{CachedBundleStorage, DEFAULT_LRU_CAPACITY, DEFAULT_MAX_CACHED_BUNDLE_SIZE};
/// In-memory [`MetadataStorage`] backend, suitable for testing and ephemeral deployments.
pub use metadata_mem::{Config as MetadataMemStorageConfig, MetadataMemStorage};
pub(crate) use store::Store;

/// The `MetadataStorage` trait defines the interface for storing and managing bundle metadata.
///
/// This trait provides a set of asynchronous methods for interacting with the metadata storage,
/// including inserting, retrieving, and updating bundle metadata. It also includes methods for
/// more complex operations such as recovering unconfirmed bundles, polling for bundles in
/// various states, and managing the lifecycle of bundles within the storage system.
///
/// Implementers of this trait are expected to provide a thread-safe and efficient implementation
/// of these methods.
#[async_trait]
pub trait MetadataStorage: Send + Sync {
    /// Retrieves the metadata for a bundle with the given `bundle_id`.
    ///
    /// # Arguments
    ///
    /// * `bundle_id` - The ID of the bundle to retrieve.
    ///
    /// # Returns
    ///
    /// A `Result` containing an `Option<Bundle>`. `Some(bundle)` if the bundle is found,
    /// `None` if it is not.
    async fn get(&self, bundle_id: &Id) -> Result<Option<Bundle>>;

    /// Inserts a new bundle's metadata into the storage.
    ///
    /// # Arguments
    ///
    /// * `bundle` - The bundle to insert.
    ///
    /// # Returns
    ///
    /// A `Result` containing a boolean indicating whether the insertion was successful.
    async fn insert(&self, bundle: &Bundle) -> Result<bool>;

    /// Replaces an existing bundle's metadata in the storage.
    ///
    /// # Arguments
    ///
    /// * `bundle` - The bundle to replace.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the replacement was successful.
    async fn replace(&self, bundle: &Bundle) -> Result<()>;

    /// Updates only the typed status columns for an existing bundle's metadata.
    ///
    /// Cheaper than `replace` because the bundle blob is not written. Use this
    /// for pure state-machine transitions where no other metadata has changed.
    async fn update_status(&self, bundle: &Bundle) -> Result<()>;

    /// Removes any metadata for the given `bundle_id` and leaves a "tombstone".
    /// A tombstone marks the bundle as deleted, preventing it from being re-inserted
    /// or processed further. This method does not error if the bundle does not exist.
    ///
    /// # Arguments
    ///
    /// * `bundle_id` - The ID of the bundle to tombstone.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
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
    /// this should return `Ok(None)`.
    ///
    /// # Arguments
    ///
    /// * `bundle_id` - The ID of the bundle to confirm.
    ///
    /// # Returns
    ///
    /// A `Result` containing an `Option<metadata::BundleMetadata>`. `Some(metadata)` if the
    /// bundle exists, `None` if it does not.
    async fn confirm_exists(&self, bundle_id: &Id) -> Result<Option<BundleMetadata>>;

    /// Final step of the startup recovery protocol. Removes all metadata
    /// entries that were not confirmed via `confirm_exists()` since the last
    /// `start_recovery()` call, and sends the removed bundles to `tx` so the
    /// BPA can perform any necessary cleanup (e.g. deleting bundle data).
    ///
    /// Non-persistent backends should treat this as a no-op.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sender to which the unconfirmed bundles will be sent.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn remove_unconfirmed(&self, tx: Sender<Bundle>) -> Result<()>;

    /// Resets all bundles with the status `BundleStatus::ForwardPending { peer, _ }` to `Waiting`.
    /// This allows the dispatcher to re-evaluate the forwarding decision for these bundles.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer for which the queue should be reset.
    ///
    /// # Returns
    ///
    /// A `Result` containing the number of bundles that were reset.
    async fn reset_peer_queue(&self, peer: u32) -> Result<u64>;

    /// Returns the next `limit` bundles, not of status `BundleStatus::New`, ordered by expiry.
    /// The receiver will hang up when it has enough bundles.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sender to which the bundles will be sent.
    /// * `limit` - The maximum number of bundles to return.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn poll_expiry(&self, tx: Sender<Bundle>, limit: usize) -> Result<()>;

    /// Returns all bundles with `BundleStatus::Waiting` status, snapshotted at the time of the call,
    /// ordered by received time. The receiver will hang up when it has enough bundles.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sender to which the bundles will be sent.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn poll_waiting(&self, tx: Sender<Bundle>) -> Result<()>;

    /// Returns bundles currently in `BundleStatus::WaitingForService` for the specified service source,
    /// ordered by received time. The receiver will hang up when it has enough bundles.
    ///
    /// # Arguments
    ///
    /// * `source` - The service endpoint for which waiting bundles should be retrieved.
    /// * `tx` - The sender to which the bundles will be sent.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn poll_service_waiting(&self, source: Eid, tx: Sender<Bundle>) -> Result<()>;

    /// Returns all bundles matching the `BundleStatus::AduFragment` status, preferably ordered by fragment offset.
    /// The receiver will hang up when it has enough bundles.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sender to which the bundles will be sent.
    /// * `status` - The `BundleStatus::AduFragment` status to filter by.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn poll_adu_fragments(&self, tx: Sender<Bundle>, status: &BundleStatus) -> Result<()>;

    /// Returns the next `limit` bundles waiting in a particular status, ordered by received time.
    /// The receiver will hang up when it has enough bundles.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sender to which the bundles will be sent.
    /// * `status` - The status to filter by.
    /// * `limit` - The maximum number of bundles to return.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn poll_pending(
        &self,
        tx: Sender<Bundle>,
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
#[async_trait]
pub trait BundleStorage: Send + Sync {
    /// Recovers bundles from the bundle storage and sends them to the provided sender.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sender to which the recovered bundles will be sent.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn recover(&self, tx: Sender<RecoveryResponse>) -> Result<()>;

    /// Loads a bundle from the bundle storage.
    ///
    /// # Arguments
    ///
    /// * `storage_name` - The name of the bundle to load.
    ///
    /// # Returns
    ///
    /// A `Result` containing an `Option<Bytes>`. `Some(bytes)` if the bundle is found,
    /// `None` if it is not.
    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>>;

    /// Saves a bundle to the bundle storage.
    ///
    /// # Arguments
    ///
    /// * `data` - The binary data of the bundle to save.
    ///
    /// # Returns
    ///
    /// A `Result` containing the name of the saved bundle.
    async fn save(&self, data: Bytes) -> Result<Arc<str>>;

    /// Overwrites existing bundle data at the given storage name.
    ///
    /// The implementation must ensure atomicity: readers see either the
    /// old data or the new data, never a partial write.
    async fn overwrite(&self, storage_name: &str, data: Bytes) -> Result<()>;

    /// Deletes a bundle from the bundle storage.
    ///
    /// # Arguments
    ///
    /// * `storage_name` - The name of the bundle to delete.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn delete(&self, storage_name: &str) -> Result<()>;
}
