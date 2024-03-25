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
        _config: &config::Config,
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
        cancel_token: tokio_util::sync::CancellationToken,
        channel: tokio::sync::mpsc::Sender<(bundle::Bundle, bool)>,
    ) -> Result<(), anyhow::Error> {
        // Bundle storage checks first
        self.bundle_storage.check(|storage_name| {
            let r = tokio::runtime::Handle::current().block_on(async {
                // Check if the metadata_storage knows about this bundle
                if self.metadata_storage.confirm_exists(storage_name).await? {
                    return Ok::<bool, anyhow::Error>(true);
                }

                // Parse the bundle first
                let data = self.bundle_storage.load(storage_name).await?;
                let Ok((bundle, valid)) = bundle::parse(data.as_bytes()) else {
                    // Drop it... garbage
                    return Ok(false);
                };

                // Write to metadata or die trying
                self.metadata_storage.store(storage_name, &bundle).await?;

                // Queue the new bundle for ingress processing
                channel.send((bundle, valid)).await?;

                // true for keep
                Ok(true)
            })?;

            // Just dumb poll the cancel token now - try to avoid mismatched state again
            Ok((!cancel_token.is_cancelled()).then_some(r))
        })?;

        if !cancel_token.is_cancelled() {
            // Now check the metadata storage
            self.metadata_storage.check(|bundle| {
                // The data associated with `bundle` has gone!

                // Just dumb poll the cancel token now - try to avoid mismatched state again
                Ok(!cancel_token.is_cancelled())
            })?;
        }

        Ok(())
    }

    pub async fn store(
        &self,
        data: Arc<Vec<u8>>,
    ) -> Result<Option<(bundle::Bundle, bool)>, anyhow::Error> {
        // Start the write to bundle storage
        let write_result = self.bundle_storage.store(data.clone());

        // Parse the bundle in parallel
        let bundle_result = bundle::parse(&data);

        // Await the result of write to bundle storage
        let storage_name = write_result.await?;

        // Check parse result
        let (bundle, valid) = match bundle_result {
            Ok(r) => r,
            Err(e) => {
                // Parse failed badly, no idea who to report to
                log::info!("Bundle parsing failed: {}", e);

                // Remove from bundle storage
                let _ = self.bundle_storage.remove(&storage_name).await;
                return Ok(None);
            }
        };

        // Write to metadata store
        match self.metadata_storage.store(&storage_name, &bundle).await {
            Ok(_) => {}
            Err(e) => {
                // This is just bad, we can't really claim to have received the bundle,
                // so just cleanup and get out
                let _ = self.bundle_storage.remove(&storage_name).await;
                return Err(e);
            }
        }

        // Return the parsed bundle
        Ok(Some((bundle, valid)))
    }
}
