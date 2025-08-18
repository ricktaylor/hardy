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

    async fn remove_unconfirmed(&self, tx: storage::Sender<bundle::Bundle>) -> Result<()>;
}

pub type ListResponse = (Arc<str>, time::OffsetDateTime);

#[async_trait]
pub trait BundleStorage: Send + Sync {
    async fn list(&self, tx: storage::Sender<ListResponse>) -> Result<()>;

    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>>;

    async fn save(&self, data: Bytes) -> Result<Arc<str>>;

    async fn delete(&self, storage_name: &str) -> Result<()>;
}
