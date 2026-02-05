use super::*;

impl Dispatcher {
    pub async fn reassemble(&self, mut bundle: bundle::Bundle) {
        let Some((storage_name, data)) = self.store.adu_reassemble(&mut bundle).await else {
            // Nothing more to do, all the fragments have yet to arrive
            return self.store.watch_bundle(bundle).await;
        };

        let metadata = metadata::BundleMetadata {
            storage_name: Some(storage_name),
            ..Default::default()
        };

        // Reparse the reconstituted bundle, for sanity

        match hardy_bpv7::bundle::ParsedBundle::parse(&data, self.key_provider()) {
            Ok(hardy_bpv7::bundle::ParsedBundle { bundle, .. }) => {
                let bundle = bundle::Bundle { metadata, bundle };
                self.store.insert_metadata(&bundle).await;

                // Box::pin breaks the type recursion; call inner directly (already in pool)
                Box::pin(self.ingest_bundle_inner(bundle, data)).await;
            }
            Err(error) => {
                // Reconstituted bundle is garbage
                debug!("Reassembled bundle is invalid: {error}");

                // TODO: Report this as a reception failure for all the fragments, if we can identify them (we can at least report the fragment we just received)

                // TODO: This is where we can wrap the damaged bundle in a "Junk Bundle Payload" and forward it to a 'lost+found' endpoint.  For now we just drop it.

                if let Some(storage_name) = metadata.storage_name {
                    self.store.delete_data(&storage_name).await;
                }
            }
        }
    }
}
