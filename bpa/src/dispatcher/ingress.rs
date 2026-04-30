use super::*;
use hardy_bpv7::status_report::ReasonCode;

impl Dispatcher {
    // Entry point for bundles received from CLAs.
    //
    // Bundle validation errors are handled internally (logged and dropped) rather
    // than returned to the CLA, since the CLA cannot fix invalid bundle content.
    //
    // # Bundle State
    //
    // - Initial status: `New`
    // - Next: `process_received_bundle()` → `ingress_bundle()` → Ingress filter → `Dispatching`
    //
    // See [Bundle State Machine Design](../../docs/bundle_state_machine_design.md)
    // for the complete state transition diagram.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub async fn receive_bundle(
        &self,
        data: Bytes,
        ingress_cla: Option<Arc<str>>,
        ingress_peer_node: Option<hardy_bpv7::eid::NodeId>,
        ingress_peer_addr: Option<cla::ClaAddress>,
    ) -> cla::Result<()> {
        metrics::counter!("bpa.bundle.received").increment(1);
        metrics::counter!("bpa.bundle.received.bytes").increment(data.len() as u64);

        let metadata = bundle::BundleMetadata {
            status: bundle::BundleStatus::New,
            read_only: bundle::ReadOnlyMetadata {
                received_at: time::OffsetDateTime::now_utc(),
                ingress_peer_node,
                ingress_peer_addr,
                ingress_cla,
                ..Default::default()
            },
            ..Default::default()
        };

        if let Some((bundle, data)) = self.process_received_bundle(data, metadata).await {
            self.ingress_bundle(bundle, data).await;
        }
        Ok(())
    }

    // Shared bundle processing: parse, validate, store, and report.
    //
    // Called from both the CLA ingress path (`receive_bundle`) and the ADU
    // reassembly path (`reassemble`). Handles all bundle validation internally
    // — invalid bundles are logged, counted, and dropped with status reports
    // where possible.
    //
    // Returns `Some((bundle, data))` for valid bundles ready for ingress,
    // or `None` if the bundle was dropped (invalid, duplicate, etc.).
    //
    // If `metadata.storage_name` is already set (reassembly case), the existing
    // stored data is used. Otherwise (CLA case), the data is saved after parsing.
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    pub(super) async fn process_received_bundle(
        &self,
        mut data: Bytes,
        mut metadata: bundle::BundleMetadata,
    ) -> Option<(bundle::Bundle, Bytes)> {
        // Fast pre-check: reject empty, BPv6, and non-CBOR-array data
        if let Err(e) = crate::cbor::precheck(&data) {
            debug!("Bundle rejected by CBOR precheck: {e}");
            metrics::counter!("bpa.bundle.received.dropped").increment(1);
            if let Some(storage_name) = &metadata.storage_name {
                self.store.delete_data(storage_name).await;
            }
            return None;
        }

        // Parse the bundle with full processing (block removal, canonicalization, BPSec)
        let (bundle, reason, report_unsupported) =
            match hardy_bpv7::bundle::RewrittenBundle::parse(&data, self.key_provider()) {
                Err(e) => {
                    debug!("Bundle parse failed: {e}");
                    metrics::counter!("bpa.bundle.received.dropped").increment(1);
                    if let Some(storage_name) = &metadata.storage_name {
                        self.store.delete_data(storage_name).await;
                    }
                    return None;
                }
                Ok(hardy_bpv7::bundle::RewrittenBundle::Valid {
                    bundle,
                    report_unsupported,
                }) => {
                    if metadata.storage_name.is_none() {
                        metadata.storage_name = Some(self.store.save_data(data.clone()).await);
                    }
                    (
                        bundle::Bundle { metadata, bundle },
                        None,
                        report_unsupported,
                    )
                }
                Ok(hardy_bpv7::bundle::RewrittenBundle::Rewritten {
                    bundle,
                    new_data,
                    report_unsupported,
                    non_canonical: _,
                }) => {
                    debug!("Received bundle has been rewritten");

                    data = Bytes::from(new_data);
                    if let Some(storage_name) = &metadata.storage_name {
                        self.store.replace_data(storage_name, data.clone()).await;
                    } else {
                        metadata.storage_name = Some(self.store.save_data(data.clone()).await);
                    }

                    (
                        bundle::Bundle { metadata, bundle },
                        None,
                        report_unsupported,
                    )
                }
                Ok(hardy_bpv7::bundle::RewrittenBundle::Invalid {
                    bundle,
                    reason,
                    error,
                }) => {
                    debug!("Invalid bundle received: {error}");

                    // Delete any pre-saved data (reassembly case)
                    if let Some(storage_name) = metadata.storage_name.take() {
                        self.store.delete_data(&storage_name).await;
                    }

                    (bundle::Bundle { metadata, bundle }, Some(reason), false)
                }
            };

        if !self.store.insert_metadata(&bundle).await {
            // Bundle with matching id already exists in the metadata store
            metrics::counter!("bpa.bundle.received.duplicate").increment(1);

            // TODO: There may be custody transfer signalling that needs to happen here

            // Drop the stored data and do not process further
            if let Some(storage_name) = &bundle.metadata.storage_name {
                self.store.delete_data(storage_name).await;
            }
            return None;
        }

        // Report we have received the bundle
        self.report_bundle_reception(
            &bundle,
            if let Some(reason) = &reason {
                *reason
            } else if report_unsupported {
                ReasonCode::BlockUnsupported
            } else {
                ReasonCode::NoAdditionalInformation
            },
        )
        .await;

        if reason.is_some() {
            // Invalid bundle — never entered the pipeline, just clean up
            self.store.tombstone_metadata(&bundle.bundle.id).await;
            metrics::counter!("bpa.bundle.received.dropped").increment(1);
            None
        } else {
            Some((bundle, data))
        }
    }

    // Run the Ingress filter, checkpoint to `Dispatching`, and route the bundle.
    //
    // # Processing Steps
    //
    // 1. Execute Ingress filter hook
    // 2. Persist any filter mutations (crash-safe ordering)
    // 3. **Checkpoint**: Transition status to `Dispatching`
    // 4. Call `process_bundle()` for routing decision
    //
    // # Crash Safety
    //
    // The checkpoint to `Dispatching` is always persisted after the Ingress
    // filter completes. On restart, bundles in `New` status re-run from this
    // function, while bundles in `Dispatching` skip directly to routing.
    //
    // See [Filter Subsystem Design](../../docs/filter_subsystem_design.md) for
    // filter execution details.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn ingress_bundle(&self, mut bundle: bundle::Bundle, mut data: Bytes) {
        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);

        // Ingress filter hook (includes bundle-validity: flags, lifetime, hop-count)
        (bundle, data) = match self
            .filter_engine
            .exec(
                filter::Hook::Ingress,
                bundle,
                data,
                self.key_provider(),
                &self.processing_pool,
            )
            .await
        {
            Ok(filter::ExecResult::Continue(mutation, mut bundle, data)) => {
                if mutation.data {
                    if let Some(storage_name) = &bundle.metadata.storage_name {
                        self.store.replace_data(storage_name, data.clone()).await;
                    }
                }
                // Always checkpoint to Dispatching (crash safety)
                metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).decrement(1.0);
                bundle.metadata.status = bundle::BundleStatus::Dispatching;
                metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.metadata.status)).increment(1.0);
                self.store.update_metadata(&bundle).await;
                (bundle, data)
            }
            Ok(filter::ExecResult::Drop(bundle, reason)) => {
                if let Some(reason) = reason {
                    return self.drop_bundle(bundle, reason).await;
                } else {
                    return self.delete_bundle(bundle).await;
                }
            }
            Err(e) => {
                error!("Ingress filter execution failed: {e}");
                return;
            }
        };

        self.process_bundle(bundle, data, self.cla_registry()).await;
    }
}
