use super::*;

impl Dispatcher {
    pub async fn reassemble(&self, bundle: &mut bundle::Bundle) -> Option<bundle::Bundle> {
        let Some((mut metadata, data)) = self.store.adu_reassemble(bundle).await else {
            // Nothing more to do, all the fragments have yet to arrive
            return None;
        };

        // Reparse the reconstituted bundle, for sanity
        match hardy_bpv7::bundle::RewrittenBundle::parse(&data, self.key_provider()) {
            Ok(hardy_bpv7::bundle::RewrittenBundle::Valid { bundle, .. }) => {
                let bundle = bundle::Bundle { metadata, bundle };
                self.store.insert_metadata(&bundle).await;
                Some(bundle)
            }
            Ok(hardy_bpv7::bundle::RewrittenBundle::Rewritten {
                bundle,
                new_data,
                non_canonical,
                ..
            }) => {
                debug!("Reassembled bundle has been rewritten");

                // Update the metadata
                metadata.non_canonical = non_canonical;

                let old_storage_name = metadata
                    .storage_name
                    .replace(self.store.save_data(new_data.into()).await);

                let bundle = bundle::Bundle { metadata, bundle };
                self.store.insert_metadata(&bundle).await;

                // And drop the original bundle data
                if let Some(old_storage_name) = old_storage_name {
                    self.store.delete_data(&old_storage_name).await;
                }
                Some(bundle)
            }
            Ok(hardy_bpv7::bundle::RewrittenBundle::Invalid { error, .. }) | Err(error) => {
                // Reconstituted bundle is garbage
                debug!("Reassembled bundle is invalid: {error}");

                if let Some(storage_name) = metadata.storage_name {
                    self.store.delete_data(&storage_name).await;
                }
                None
            }
        }
    }
}
