use super::*;
use sha2::Digest;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = core::result::Result<T, Error>;

#[async_trait]
pub trait MetadataStorage: Send + Sync {
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool>,
    ) -> Result<()>;

    fn restart(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool>,
    ) -> Result<()>;

    async fn store(&self, metadata: &bundle::Metadata, bundle: &bundle::Bundle) -> Result<bool>;

    async fn set_bundle_status(
        &self,
        storage_name: &str,
        status: bundle::BundleStatus,
    ) -> Result<()>;

    async fn remove(&self, storage_name: &str) -> Result<()>;

    async fn confirm_exists(&self, storage_name: &str, hash: &[u8]) -> Result<bool>;

    async fn begin_replace(&self, storage_name: &str, hash: &[u8]) -> Result<()>;

    async fn commit_replace(&self, storage_name: &str, hash: &[u8]) -> Result<()>;

    async fn get_waiting_bundles(
        &self,
        limit: time::OffsetDateTime,
    ) -> Result<Vec<(bundle::Metadata, bundle::Bundle, time::OffsetDateTime)>>;
}

pub type DataRef = std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>;

#[async_trait]
pub trait BundleStorage: Send + Sync {
    fn hash(&self, data: &[u8]) -> Vec<u8> {
        sha2::Sha256::digest(data).to_vec()
    }

    #[allow(clippy::type_complexity)]
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(&str, &[u8], Option<time::OffsetDateTime>) -> Result<bool>,
    ) -> Result<()>;

    async fn load(&self, storage_name: &str) -> Result<DataRef>;

    async fn store(&self, data: Vec<u8>) -> Result<String>;

    async fn remove(&self, storage_name: &str) -> Result<()>;

    async fn replace(&self, storage_name: &str, data: Vec<u8>) -> Result<()>;
}
