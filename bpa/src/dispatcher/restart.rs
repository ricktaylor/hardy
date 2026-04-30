use super::*;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub(crate) async fn restart_bundle(
        self: &Arc<Self>,
        storage_name: Arc<str>,
        file_time: time::OffsetDateTime,
    ) {
        let Some(data) = self.store.load_data(&storage_name).await else {
            // Data has gone while we were restarting — the reaper hasn't started,
            // so this is data loss. Safe because metadata recovery will report it
            // if the bundle is in the metadata store.
            return;
        };

        // Validate the stored bundle data is not corrupt. We use ParsedBundle
        // (Preserve mode) rather than RewrittenBundle because the bundle was
        // already fully processed at ingress — restart should verify integrity
        // and resume, not re-apply block removal or canonicalization.
        let bundle = match hardy_bpv7::bundle::ParsedBundle::parse(&data, self.key_provider()) {
            Ok(parsed) => parsed.bundle,
            Err(e) => {
                // Can't extract a bundle ID, so we can't check or clean up
                // metadata here. Any orphaned metadata referencing this
                // storage_name will be caught by metadata_storage_recovery.
                warn!("Corrupt bundle data found: {storage_name}, {e}");
                self.store.delete_data(&storage_name).await;
                metrics::counter!("bpa.restart.junk").increment(1);
                return;
            }
        };

        // Reconcile with metadata store
        if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
            if metadata.storage_name.as_ref() != Some(&storage_name) {
                // Metadata references a different copy — this one is a duplicate
                if metadata.storage_name.is_none() {
                    warn!("Duplicate copy of processed bundle data found: {storage_name}");
                } else {
                    warn!(
                        "Duplicate bundle data found: {storage_name} != {:?}",
                        metadata.storage_name.as_ref()
                    );
                }
                self.store.delete_data(&storage_name).await;
                metrics::counter!("bpa.restart.duplicate").increment(1);
                return;
            }

            // Resume processing based on checkpoint status
            let bundle = bundle::Bundle { metadata, bundle };
            match &bundle.metadata.status {
                bundle::BundleStatus::New => {
                    // Ingress filter not yet complete — run full ingress
                    self.ingress_bundle(bundle, data).await;
                }
                bundle::BundleStatus::Dispatching => {
                    // Ingress filter done — enqueue for routing
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                    self.dispatch_bundle(bundle).await;
                }
                bundle::BundleStatus::ForwardPending { .. } => {
                    // Peer ID is stale after restart — reset to Waiting
                    let mut bundle = bundle;
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                    self.store
                        .update_status(&mut bundle, &bundle::BundleStatus::Waiting)
                        .await;
                }
                // Other statuses are handled by their respective recovery mechanisms:
                // - Waiting: poll_waiting recovery
                // - WaitingForService: poll_service_waiting on service re-registration
                // - AduFragment: fragment reassembly polling
                _ => {
                    metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                }
            }
        } else {
            // Orphan — data exists but no metadata. Run the full receive
            // pipeline (RewrittenBundle parse, block removal, canonicalization,
            // storage, reporting, and Ingress filter).
            let metadata = bundle::BundleMetadata {
                status: bundle::BundleStatus::New,
                storage_name: Some(storage_name),
                read_only: bundle::ReadOnlyMetadata {
                    received_at: file_time,
                    ..Default::default()
                },
                ..Default::default()
            };

            if let Some((bundle, data)) = self.process_received_bundle(data, metadata).await {
                self.ingress_bundle(bundle, data).await;
            }
            metrics::counter!("bpa.restart.orphan").increment(1);
        }
    }
}
