use super::*;

pub type Error = Box<dyn core::error::Error + Send + Sync>;
pub type Result<T> = core::result::Result<T, Error>;
pub type Sender = tokio::sync::mpsc::Sender<bundle::Bundle>;

#[async_trait]
pub trait MetadataStorage: Send + Sync {
    async fn load(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<Option<bundle::Bundle>>;

    async fn store(&self, bundle: &bundle::Bundle) -> Result<bool>;

    async fn remove(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<()>;

    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<bundle::Bundle>>;

    async fn remove_unconfirmed_bundles(&self, tx: Sender) -> Result<()>;
}

pub type ListResponse = (Arc<str>, Option<time::OffsetDateTime>);

#[async_trait]
pub trait BundleStorage: Send + Sync {
    async fn list(&self, tx: tokio::sync::mpsc::Sender<ListResponse>) -> Result<()>;

    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>>;

    async fn store(&self, data: Bytes) -> Result<Arc<str>>;

    async fn remove(&self, storage_name: &str) -> Result<()>;
}
