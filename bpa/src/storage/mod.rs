use super::*;
use lru::LruCache;
use std::collections::BTreeSet;
use std::sync::Mutex;

pub type Error = Box<dyn core::error::Error + Send + Sync>;
pub type Result<T> = core::result::Result<T, Error>;
pub type Sender<T> = flume::Sender<T>;

pub mod bundle_mem;
pub mod metadata_mem;

pub(crate) mod adu_reassembly;
pub(crate) mod channel;
pub(crate) mod recover;
pub(crate) mod store;

mod reaper;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Config {
    pub lru_capacity: std::num::NonZeroUsize,
    pub max_cached_bundle_size: std::num::NonZeroUsize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lru_capacity: std::num::NonZeroUsize::new(1024).unwrap(),
            max_cached_bundle_size: std::num::NonZeroUsize::new(16 * 1024).unwrap(),
        }
    }
}

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
    /// A `Result` containing an `Option<bundle::Bundle>`. `Some(bundle)` if the bundle is found,
    /// `None` if it is not.
    async fn get(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<Option<bundle::Bundle>>;

    /// Inserts a new bundle's metadata into the storage.
    ///
    /// # Arguments
    ///
    /// * `bundle` - The bundle to insert.
    ///
    /// # Returns
    ///
    /// A `Result` containing a boolean indicating whether the insertion was successful.
    async fn insert(&self, bundle: &bundle::Bundle) -> Result<bool>;

    /// Replaces an existing bundle's metadata in the storage.
    ///
    /// # Arguments
    ///
    /// * `bundle` - The bundle to replace.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the replacement was successful.
    async fn replace(&self, bundle: &bundle::Bundle) -> Result<()>;

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
    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<()>;

    /// Confirms that a bundle exists in the storage and returns its metadata.
    ///
    /// # Arguments
    ///
    /// * `bundle_id` - The ID of the bundle to confirm.
    ///
    /// # Returns
    ///
    /// A `Result` containing an `Option<metadata::BundleMetadata>`. `Some(metadata)` if the
    /// bundle exists, `None` if it does not.
    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<metadata::BundleMetadata>>;

    /// Initiates the recovery process for the metadata storage.
    /// This method is responsible for restoring the storage to a consistent state.
    async fn start_recovery(&self);

    /// Removes all unconfirmed bundles from the storage and sends them to the provided sender.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sender to which the unconfirmed bundles will be sent.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn remove_unconfirmed(&self, tx: Sender<bundle::Bundle>) -> Result<()>;

    /// Resets all bundles with the status `BundleStatus::ForwardPending { peer, _ }` to `Waiting`.
    /// This allows the dispatcher to re-evaluate the forwarding decision for these bundles.
    ///
    /// # Arguments
    ///
    /// * `peer` - The peer for which the queue should be reset.
    ///
    /// # Returns
    ///
    /// A `Result` containing a boolean indicating whether any bundles were reset.
    async fn reset_peer_queue(&self, peer: u32) -> Result<bool>;

    /// Returns the next `limit` bundles, not of status `BundleStatus::Dispatching`, ordered by expiry.
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
    async fn poll_expiry(&self, tx: Sender<bundle::Bundle>, limit: usize) -> Result<()>;

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
    async fn poll_waiting(&self, tx: Sender<bundle::Bundle>) -> Result<()>;

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
    async fn poll_adu_fragments(
        &self,
        tx: Sender<bundle::Bundle>,
        status: &metadata::BundleStatus,
    ) -> Result<()>;

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
        tx: storage::Sender<bundle::Bundle>,
        status: &metadata::BundleStatus,
        limit: usize,
    ) -> storage::Result<()>;
}

pub type RecoveryResponse = (Arc<str>, time::OffsetDateTime);

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

// Storage helper
pub(crate) struct Store {
    tasks: hardy_async::task_pool::TaskPool,
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    bundle_storage: Arc<dyn storage::BundleStorage>,
    bundle_cache: Mutex<LruCache<Arc<str>, Bytes>>,
    reaper_cache: Arc<Mutex<BTreeSet<reaper::CacheEntry>>>,
    reaper_wakeup: Arc<hardy_async::Notify>,

    // Config
    max_cached_bundle_size: usize,
    reaper_cache_size: usize,
}
