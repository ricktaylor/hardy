use super::*;
use metadata::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = core::result::Result<T, Error>;
pub type Sender = tokio::sync::mpsc::Sender<(metadata::BundleMetadata, hardy_bpv7::bundle::Bundle)>;

#[async_trait]
pub trait MetadataStorage: Send + Sync {
    async fn load(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<(metadata::BundleMetadata, hardy_bpv7::bundle::Bundle)>>;

    async fn store(
        &self,
        metadata: &BundleMetadata,
        bundle: &hardy_bpv7::bundle::Bundle,
    ) -> Result<bool>;

    async fn get_bundle_status(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<BundleStatus>>;

    async fn set_bundle_status(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        status: &BundleStatus,
    ) -> Result<()>;

    async fn remove(&self, bundle_id: &hardy_bpv7::bundle::Id) -> Result<()>;

    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> Result<Option<BundleMetadata>>;

    async fn get_unconfirmed_bundles(&self, tx: Sender) -> Result<()>;
}

pub type ListResponse = (std::sync::Arc<str>, Option<time::OffsetDateTime>);

#[async_trait]
pub trait BundleStorage: Send + Sync {
    async fn list(&self, tx: tokio::sync::mpsc::Sender<ListResponse>) -> Result<()>;

    async fn load(&self, storage_name: &str) -> Result<Option<Bytes>>;

    async fn store(&self, data: Bytes) -> Result<std::sync::Arc<str>>;

    async fn remove(&self, storage_name: &str) -> Result<()>;
}
