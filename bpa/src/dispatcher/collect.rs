use super::*;

pub struct CollectResponse {
    pub bundle_id: String,
    pub expiry: time::OffsetDateTime,
    pub app_ack_requested: bool,
    pub data: Vec<u8>,
}

impl Dispatcher {
    #[instrument(skip(self))]
    pub async fn collect(
        &self,
        destination: bpv7::Eid,
        bundle_id: String,
    ) -> Result<Option<CollectResponse>, Error> {
        // Lookup bundle
        let Some(bundle) = self
            .store
            .load(&bpv7::BundleId::from_key(&bundle_id)?)
            .await?
        else {
            return Ok(None);
        };

        if bundle.bundle.destination != destination || bundle.has_expired() {
            return Ok(None);
        }

        // Double check that we are returning something valid
        let metadata::BundleStatus::CollectionPending = &bundle.metadata.status else {
            return Ok(None);
        };

        // Get the data!
        let Some(data) = self.load_data(&bundle).await? else {
            // Bundle data was deleted sometime during processing
            return Ok(None);
        };

        // By the time we get here, we're safe to report delivery
        self.report_bundle_delivery(&bundle).await?;

        // Prepare the response
        let response = CollectResponse {
            bundle_id: bundle.bundle.id.to_key(),
            data: data.as_ref().as_ref().to_vec(),
            expiry: bundle.expiry(),
            app_ack_requested: bundle.bundle.flags.app_ack_requested,
        };

        // And we are done with the bundle
        self.drop_bundle(bundle, None).await?;

        Ok(Some(response))
    }

    #[instrument(skip(self))]
    pub async fn poll_for_collection(
        &self,
        destination: bpv7::Eid,
        tx: tokio::sync::mpsc::Sender<metadata::Bundle>,
    ) -> Result<(), Error> {
        self.store.poll_for_collection(destination, tx).await
    }
}
