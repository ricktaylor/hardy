use super::*;
use metadata::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = core::result::Result<T, Error>;
pub type Sender = tokio::sync::mpsc::Sender<(metadata::BundleMetadata, bpv7::Bundle)>;

#[async_trait]
pub trait MetadataStorage: Send + Sync {
    async fn load(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> Result<Option<(metadata::BundleMetadata, bpv7::Bundle)>>;

    async fn store(&self, metadata: &BundleMetadata, bundle: &bpv7::Bundle) -> Result<bool>;

    async fn get_bundle_status(&self, bundle_id: &bpv7::BundleId) -> Result<Option<BundleStatus>>;

    async fn set_bundle_status(
        &self,
        bundle_id: &bpv7::BundleId,
        status: &BundleStatus,
    ) -> Result<()>;

    async fn remove(&self, bundle_id: &bpv7::BundleId) -> Result<()>;

    async fn confirm_exists(&self, bundle_id: &bpv7::BundleId) -> Result<Option<BundleMetadata>>;

    async fn get_waiting_bundles(&self, limit: time::OffsetDateTime, tx: Sender) -> Result<()>;

    async fn get_unconfirmed_bundles(&self, tx: Sender) -> Result<()>;

    async fn poll_for_collection(&self, destination: &bpv7::Eid, tx: Sender) -> Result<()>;
}

pub type DataRef = std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>;
pub type ListResponse = (std::sync::Arc<str>, Option<time::OffsetDateTime>);

#[async_trait]
pub trait BundleStorage: Send + Sync {
    async fn list(&self, tx: tokio::sync::mpsc::Sender<ListResponse>) -> Result<()>;

    async fn load(&self, storage_name: &str) -> Result<Option<DataRef>>;

    async fn store(&self, data: &[u8]) -> Result<std::sync::Arc<str>>;

    async fn remove(&self, storage_name: &str) -> Result<()>;
}
