use super::*;

impl Dispatcher {
    #[instrument(skip(self))]
    pub async fn collect(
        &self,
        destination: &bpv7::Eid,
        bundle_id: &bpv7::BundleId,
    ) -> Result<Option<service::Bundle>, Error> {
        // Lookup bundle
        let Some(bundle) = self.store.load(bundle_id).await? else {
            return Ok(None);
        };

        // Double check that we are returning something valid
        let BundleStatus::CollectionPending = &bundle.metadata.status else {
            return Ok(None);
        };

        if &bundle.bundle.destination != destination || bundle.has_expired() {
            return Ok(None);
        }

        // Get the data!
        let Some(data) = self.load_data(&bundle).await? else {
            // Bundle data was deleted sometime during processing
            return Ok(None);
        };

        // By the time we get here, we're safe to report delivery
        self.report_bundle_delivery(&bundle).await?;

        // Prepare the response
        let response = service::Bundle {
            expiry: bundle.expiry(),
            ack_requested: bundle.bundle.flags.app_ack_requested,
            id: bundle.bundle.id.clone(),
            payload: data.as_ref().as_ref().into(),
        };

        // And we are done with the bundle
        self.drop_bundle(bundle, None).await?;

        Ok(Some(response))
    }

    #[instrument(skip(self))]
    pub async fn poll_for_collection(
        &self,
        destination: &bpv7::Eid,
        tx: storage::Sender,
    ) -> Result<(), Error> {
        self.store.poll_for_collection(destination, tx).await
    }
}
