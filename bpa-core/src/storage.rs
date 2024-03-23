use super::*;

pub trait MetadataStorage {
    fn store(
        &self,
        storage_name: &str,
        bundle: &bundle::Bundle,
    ) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send;

    fn remove(
        &self,
        storage_name: &str,
    ) -> impl std::future::Future<Output = Result<bool, anyhow::Error>> + Send;
}

pub trait BundleStorage {
    fn check<M, F>(
        &self,
        metadata: std::sync::Arc<M>,
        cancel_token: tokio_util::sync::CancellationToken,
        f: F,
    ) -> impl std::future::Future<Output = Result<(), anyhow::Error>> + Send
    where
        M: storage::MetadataStorage + Send + Sync,
        F: FnMut(&str, &[u8]) -> Result<bool, anyhow::Error> + Send;

    fn store(
        &self,
        data: std::sync::Arc<Vec<u8>>,
    ) -> impl std::future::Future<Output = Result<String, anyhow::Error>> + Send;

    fn remove(
        &self,
        storage_name: &str,
    ) -> impl std::future::Future<Output = Result<bool, anyhow::Error>> + Send;
}
