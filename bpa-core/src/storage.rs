use super::*;

#[async_trait]
pub trait MetadataStorage: Send + Sync {
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error>;

    async fn store(
        &self,
        status: bundle::BundleStatus,
        storage_name: &str,
        hash: &[u8],
        bundle: &bundle::Bundle,
    ) -> Result<bundle::Metadata, anyhow::Error>;

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error>;

    async fn confirm_exists(
        &self,
        storage_name: &str,
        hash: Option<&[u8]>,
    ) -> Result<bool, anyhow::Error>;
}

#[async_trait]
pub trait BundleStorage: Send + Sync {
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(&str) -> Result<Option<bool>, anyhow::Error>,
    ) -> Result<(), anyhow::Error>;

    async fn load(
        &self,
        storage_name: &str,
    ) -> Result<std::sync::Arc<dyn AsRef<[u8]>>, anyhow::Error>;

    async fn store(&self, data: Vec<u8>) -> Result<String, anyhow::Error>;

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error>;
}
