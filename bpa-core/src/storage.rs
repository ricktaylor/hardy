use super::*;

pub trait MetadataStorage {
    fn check<F>(&self, f: F) -> Result<(), anyhow::Error>
    where
        F: FnMut(bundle::Bundle) -> Result<bool, anyhow::Error>;

    fn store(
        &self,
        storage_name: &str,
        bundle: &bundle::Bundle,
    ) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send;

    fn remove(
        &self,
        storage_name: &str,
    ) -> impl std::future::Future<Output = Result<bool, anyhow::Error>> + Send;

    fn confirm_exists(
        &self,
        storage_name: &str,
    ) -> impl std::future::Future<Output = Result<bool, anyhow::Error>> + Send;
}

pub trait BundleData {
    fn as_bytes(&self) -> &[u8];
}

impl BundleData for Vec<u8> {
    fn as_bytes(&self) -> &[u8] {
        self
    }
}

pub trait BundleStorage {
    fn check<F>(&self, f: F) -> Result<(), anyhow::Error>
    where
        F: FnMut(&str) -> Result<Option<bool>, anyhow::Error>;

    fn load(
        &self,
        storage_name: &str,
    ) -> impl std::future::Future<Output = Result<std::sync::Arc<Box<dyn BundleData>>, anyhow::Error>>
           + Send;

    fn store(
        &self,
        data: std::sync::Arc<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<String, anyhow::Error>> + Send;

    fn remove(
        &self,
        storage_name: &str,
    ) -> impl std::future::Future<Output = Result<bool, anyhow::Error>> + Send;
}
