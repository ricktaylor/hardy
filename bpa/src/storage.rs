use super::*;

pub type Error = Box<dyn core::error::Error + Send + Sync>;
pub type Result<T> = core::result::Result<T, Error>;
pub type Sender<T> = flume::Sender<T>;

#[async_trait]
pub trait MetadataStorage: Send + Sync {
    async fn get(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<Option<bundle::Bundle>>;

    async fn insert(&self, bundle: &bundle::Bundle) -> Result<bool>;

    async fn replace(&self, bundle: &bundle::Bundle) -> Result<()>;

    /// Remove any metadata for [`bundle_id`] and leave a tombstone.  Does not error if the bundle does not exist
    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<()>;

    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<metadata::BundleMetadata>>;

    async fn start_recovery(&self);

    async fn remove_unconfirmed(&self, tx: Sender<bundle::Bundle>) -> Result<()>;

    /// Reset all bundles ForwardPending { peer, _ } to Waiting so that the dispatcher will re-evaluate
    async fn reset_peer_queue(&self, peer: u32) -> Result<bool>;

    /// Return the next `limit` bundles (ignore Dispatching), ordered by expiry.  The receiver will hangup when it has enough
    async fn poll_expiry(&self, tx: Sender<bundle::Bundle>, limit: usize) -> Result<()>;

    /// Return all bundles waiting to forward, ordered by received time.  The receiver will hangup when it has enough
    async fn poll_waiting(&self, tx: Sender<bundle::Bundle>) -> Result<()>;

    /// Return the next `limit` bundles waiting in a particular state, ordered by received time.  The receiver will hangup when it has enough
    async fn poll_pending(
        &self,
        tx: storage::Sender<bundle::Bundle>,
        state: &metadata::BundleStatus,
        limit: usize,
    ) -> storage::Result<()>;
}

pub type RecoveryResponse = (Arc<str>, time::OffsetDateTime);

#[async_trait]
pub trait BundleStorage: Send + Sync {
    async fn recover(&self, tx: Sender<RecoveryResponse>) -> Result<()>;

    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>>;

    async fn save(&self, data: Bytes) -> Result<Arc<str>>;

    async fn delete(&self, storage_name: &str) -> Result<()>;
}
