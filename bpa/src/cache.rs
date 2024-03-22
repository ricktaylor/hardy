use super::*;
use std::sync::Arc;

pub struct Cache<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    metadata_storage: Arc<M>,
    bundle_storage: Arc<B>,
}

impl<M, B> Cache<M, B>
where
    M: storage::MetadataStorage + Send + Sync,
    B: storage::BundleStorage + Send + Sync,
{
    pub fn new(
        config: &config::Config,
        metadata_storage: Arc<M>,
        bundle_storage: Arc<B>,
    ) -> Arc<Self> {
        Arc::new(Self {
            metadata_storage,
            bundle_storage,
        })
    }

    pub async fn check(
        &self,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> Result<(), anyhow::Error> {
        self.bundle_storage
            .check(
                self.metadata_storage.clone(),
                cancel_token,
                |storage_name, data| {
                    let metadata_storage = self.metadata_storage.clone();
                    async move {
                        // Bundle in bundle_storage, but not in metadata_storage

                        // Parse the bundle first
                        let Ok(bundle) = bundle::parse(&data) else {
                            // Drop it... garbage
                            return Ok(false);
                        };

                        // Write to metadata or die trying
                        metadata_storage.store(&storage_name, &bundle).await?;

                        todo!();

                        // true for keep
                        Ok(true)
                    }
                },
            )
            .await
    }

    pub async fn store(&self, data: Arc<Vec<u8>>) -> Result<bundle::Bundle, anyhow::Error> {
        // Start the write to bundle storage
        let write_result = self.bundle_storage.store(data.clone());

        // Parse the bundle in parallel
        let bundle_result = bundle::parse(&data);

        // Await the result of write to bundle storage
        let storage_name = write_result.await?;

        // Check parse result
        let bundle = match bundle_result {
            Ok(r) => r,
            Err(e) => {
                // Parse failed badly, no idea who to report to
                // Remove from bundle storage
                self.bundle_storage.remove(&storage_name).await;
                return Err(e);
            }
        };

        // Write to metadata store
        match self.metadata_storage.store(&storage_name, &bundle).await {
            Ok(_) => {}
            Err(e) => {
                // This is just bad, we can't really claim to have received the bundle,
                // so just cleanup and get out
                self.bundle_storage.remove(&storage_name).await;
                return Err(e);
            }
        }

        // Return the parsed bundle
        Ok(bundle)
    }
}
