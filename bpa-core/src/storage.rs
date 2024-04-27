use super::*;
use sha2::Digest;

#[async_trait]
pub trait MetadataStorage: Send + Sync {
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error>;

    fn restart(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error>;

    async fn store(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
    ) -> Result<(), anyhow::Error>;

    async fn set_bundle_status(
        &self,
        storage_name: &str,
        status: bundle::BundleStatus,
    ) -> Result<bool, anyhow::Error>;

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error>;

    async fn confirm_exists(&self, storage_name: &str, hash: &[u8]) -> Result<bool, anyhow::Error>;

    async fn begin_replace(&self, storage_name: &str, hash: &[u8]) -> Result<bool, anyhow::Error>;

    async fn commit_replace(&self, storage_name: &str, hash: &[u8]) -> Result<bool, anyhow::Error>;
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
        f: &mut dyn FnMut(&str, &[u8], Option<time::OffsetDateTime>) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error>;

    async fn load(&self, storage_name: &str) -> Result<DataRef, anyhow::Error>;

    async fn store(&self, data: Vec<u8>) -> Result<String, anyhow::Error>;

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error>;

    async fn replace(&self, storage_name: &str, data: Vec<u8>) -> Result<(), anyhow::Error>;
}
