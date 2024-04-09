use std::time::SystemTime;

use super::*;

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
    ) -> Result<(), anyhow::Error>;

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
        f: &mut dyn FnMut(&str, Option<time::OffsetDateTime>) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error>;

    async fn load(
        &self,
        storage_name: &str,
    ) -> Result<std::sync::Arc<dyn AsRef<[u8]>>, anyhow::Error>;

    async fn store(&self, data: Vec<u8>) -> Result<String, anyhow::Error>;

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error>;
}
