use super::*;
use hardy_async::sync::Mutex;
use hardy_bpv7::eid::Eid;
use lru::LruCache;

// For bundle_cache we use hardy_async::sync::spin::Mutex because:
// 1. All operations are O(1): peek, put, pop
// 2. Critical sections are very short (LRU HashMap lookups)
// 3. No blocking/sleeping/syscalls while holding lock
// 4. Avoids OS mutex overhead on hot path
//
// Other caches (metadata_mem, bundle_mem) use hardy_async::sync::Mutex because
// they perform O(n) iteration while holding the lock.

/// Boxed error type used by storage trait methods.
pub type Error = Box<dyn core::error::Error + Send + Sync>;
/// Result alias for storage operations.
pub type Result<T> = core::result::Result<T, Error>;
/// Channel sender used to stream results from storage polling methods.
pub type Sender<T> = flume::Sender<T>;

/// In-memory [`BundleStorage`] backend, suitable for testing and ephemeral deployments.
pub mod bundle_mem;
/// In-memory [`MetadataStorage`] backend, suitable for testing and ephemeral deployments.
pub mod metadata_mem;

pub(crate) mod adu_reassembly;
pub(crate) mod channel;
pub(crate) mod store;

mod reaper;

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

    /// Updates only the typed status columns for an existing bundle's metadata.
    ///
    /// Cheaper than `replace` because the bundle blob is not written. Use this
    /// for pure state-machine transitions where no other metadata has changed.
    async fn update_status(&self, bundle: &bundle::Bundle) -> Result<()>;

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

    /// Part of the startup recovery protocol. Called once per bundle after
    /// `mark_unconfirmed()` as the BPA walks the bundle store and finds data on
    /// disk. Confirms that the metadata entry for this bundle is still wanted,
    /// and returns its metadata so the BPA can resume processing.
    ///
    /// For persistent backends (e.g. SQLite), this removes the bundle from the
    /// "unconfirmed" set populated by `mark_unconfirmed()`. Any entries still in
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
    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<bundle::BundleMetadata>>;

    /// Begins the startup recovery protocol by marking all existing metadata
    /// entries as unconfirmed. The BPA then calls `confirm_exists()` for each
    /// bundle it finds in the bundle store, and finally calls
    /// `remove_unconfirmed()` to clean up any orphaned metadata.
    ///
    /// Non-persistent backends should treat this as a no-op.
    async fn mark_unconfirmed(&self);

    /// Final step of the startup recovery protocol. Removes all metadata
    /// entries that were not confirmed via `confirm_exists()` since the last
    /// `mark_unconfirmed()` call, and sends the removed bundles to `tx` so the
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
    async fn poll_service_waiting(&self, source: Eid, tx: Sender<bundle::Bundle>) -> Result<()>;

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
        status: &bundle::BundleStatus,
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
        status: &bundle::BundleStatus,
        limit: usize,
    ) -> storage::Result<()>;
}

/// A recovered bundle entry: `(storage_name, creation_time)`.
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
    /// Walk all persisted bundle data and stream each entry to the channel.
    ///
    /// Sends `(storage_name, file_time)` for every bundle found. Skips
    /// incomplete writes (`.tmp` files, zero-byte placeholders).
    ///
    /// # Arguments
    ///
    /// * `tx` - The channel sender to which each `(storage_name, file_time)` pair is sent.
    ///
    /// # Returns
    ///
    /// A `Result` indicating whether the operation was successful.
    async fn walk(&self, tx: Sender<RecoveryResponse>) -> Result<()>;

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

/// Default LRU cache capacity (number of entries).
pub const DEFAULT_LRU_CAPACITY: core::num::NonZeroUsize =
    core::num::NonZeroUsize::new(1024).unwrap();

/// Default maximum bundle size (in bytes) eligible for caching.
pub const DEFAULT_MAX_CACHED_BUNDLE_SIZE: core::num::NonZeroUsize =
    core::num::NonZeroUsize::new(16 * 1024).unwrap();

/// Bundles the LRU and its size threshold together so that
/// [`Store`] only needs a single `Option` field.
struct BundleCache {
    // Using sync::spin::Mutex - see comment at top of file
    lru: hardy_async::sync::spin::Mutex<LruCache<Arc<str>, Bytes>>,
    max_bundle_size: usize,
}

// Storage helper
pub(crate) struct Store {
    tasks: hardy_async::TaskPool,
    metadata_storage: Arc<dyn storage::MetadataStorage>,
    bundle_storage: Arc<dyn storage::BundleStorage>,

    // None when the bundle storage backend is already in-memory (avoids double-caching).
    bundle_cache: Option<BundleCache>,

    reaper_cache: Arc<Mutex<BTreeSet<reaper::CacheEntry>>>,
    reaper_wakeup: Arc<hardy_async::Notify>,

    // Config
    reaper_cache_size: usize,
}
