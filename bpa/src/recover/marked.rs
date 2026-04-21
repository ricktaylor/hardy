use futures::{FutureExt, join, select_biased};
use hardy_bpv7::bundle::{Bundle as Bpv7Bundle, RewrittenBundle};
use hardy_bpv7::status_report::ReasonCode;
use trace_err::*;
use tracing::warn;

use super::{Confirmed, Marked, Recovery};
use crate::bundle::{Bundle, BundleMetadata, BundleStatus, ReadOnlyMetadata, Stored};
use crate::otel_metrics::{reason_label, status_label};
use crate::storage::RecoveryResponse;
use crate::{Arc, Bytes};

impl<'a> Recovery<'a, Marked> {
    /// Phase 2: Walk bundle storage and reconcile each bundle against metadata.
    pub(crate) async fn reconcile(self) -> Recovery<'a, Confirmed> {
        let cancel_token = self.store.cancel_token().clone();
        let (tx, rx) = flume::bounded::<RecoveryResponse>(16);

        join!(
            async {
                self.store
                    .walk_bundles(tx)
                    .await
                    .trace_expect("Bundle storage recover failed");
            },
            async {
                loop {
                    select_biased! {
                        r = rx.recv_async().fuse() => match r {
                            Err(_) => break,
                            Ok((storage_name, file_time)) => {
                                self.recover_bundle(storage_name, file_time).await;
                            }
                        },
                        _ = cancel_token.cancelled().fuse() => {
                            break;
                        }
                    }
                }
            }
        );

        self.transition()
    }

    /// Recover a single bundle found during the storage walk.
    async fn recover_bundle(&self, storage_name: Arc<str>, file_time: time::OffsetDateTime) {
        let Some(data) = self.store.load_data(&storage_name).await.ok().flatten() else {
            metrics::counter!("bpa.restart.lost").increment(1);
            return;
        };

        match RewrittenBundle::parse(&data, self.dispatcher.key_provider()) {
            Ok(RewrittenBundle::Valid {
                bundle,
                report_unsupported,
            }) => {
                self.recover_valid(bundle, report_unsupported, storage_name, data, file_time)
                    .await;
            }
            Ok(RewrittenBundle::Rewritten {
                bundle,
                new_data,
                report_unsupported,
                non_canonical: _,
            }) => {
                self.recover_rewritten(
                    bundle,
                    new_data,
                    report_unsupported,
                    storage_name,
                    file_time,
                )
                .await;
            }
            Ok(RewrittenBundle::Invalid {
                bundle,
                reason,
                error,
            }) => {
                self.discard_invalid(bundle, reason, error, storage_name, file_time)
                    .await;
            }
            Err(e) => {
                self.discard_junk(&storage_name, e).await;
            }
        }
    }

    /// Recover a valid bundle.
    async fn recover_valid(
        &self,
        bundle: Bpv7Bundle,
        report_unsupported: bool,
        storage_name: Arc<str>,
        data: Bytes,
        file_time: time::OffsetDateTime,
    ) {
        if let Some(existing) = self.store.confirm_exists(&bundle.id).await.ok().flatten() {
            if existing.storage_name() != &storage_name {
                self.delete_duplicate(&storage_name, existing.storage_name())
                    .await;
            } else {
                self.resume(existing, data).await;
            }
        } else {
            self.ingest_orphan(bundle, report_unsupported, storage_name, data, file_time)
                .await;
        }
    }

    /// Resume processing a bundle that has existing metadata.
    async fn resume(&self, mut bundle: Bundle<Stored>, data: Bytes) {
        match &bundle.metadata.status {
            BundleStatus::New => {
                self.dispatcher.ingest_bundle(bundle, data).await;
            }
            BundleStatus::Dispatching => {
                metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.metadata.status)).increment(1.0);
                self.dispatcher.dispatch_bundle(bundle).await;
            }
            BundleStatus::ForwardPending { .. } => {
                metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.metadata.status)).increment(1.0);
                let _ = self
                    .store
                    .update_status(&mut bundle, &BundleStatus::Waiting)
                    .await;
            }
            _ => {
                metrics::gauge!("bpa.bundle.status", "state" => status_label(&bundle.metadata.status))
                    .increment(1.0);
            }
        }
    }

    /// Ingest an orphan bundle (data exists but no metadata).
    async fn ingest_orphan(
        &self,
        bundle: Bpv7Bundle,
        report_unsupported: bool,
        storage_name: Arc<str>,
        data: Bytes,
        file_time: time::OffsetDateTime,
    ) {
        let bundle = Bundle {
            metadata: BundleMetadata {
                read_only: ReadOnlyMetadata {
                    received_at: file_time,
                    ..Default::default()
                },
                ..Default::default()
            },
            bundle,
            state: Stored { storage_name },
        };

        if !self.store.insert_metadata(&bundle).await.unwrap_or(false) {
            let _ = self.store.delete_data(bundle.storage_name()).await;
            metrics::counter!("bpa.restart.orphan_tombstoned").increment(1);
            return;
        }

        let reason = if report_unsupported {
            ReasonCode::BlockUnsupported
        } else {
            ReasonCode::NoAdditionalInformation
        };
        self.dispatcher
            .report_bundle_reception(&bundle, reason)
            .await;

        self.dispatcher.ingest_bundle(bundle, data).await;
        metrics::counter!("bpa.restart.orphan").increment(1);
    }

    /// Recover a non-canonical (rewritten) bundle.
    async fn recover_rewritten(
        &self,
        bundle: Bpv7Bundle,
        new_data: Box<[u8]>,
        report_unsupported: bool,
        storage_name: Arc<str>,
        file_time: time::OffsetDateTime,
    ) {
        warn!("Bundle in non-canonical format found: {storage_name}");

        let _ = self.store.delete_data(&storage_name).await;

        let exists =
            if let Some(existing) = self.store.confirm_exists(&bundle.id).await.ok().flatten() {
                if existing.storage_name() != &storage_name {
                    if existing.storage_name().is_empty() {
                        warn!("Non-canonical copy of processed bundle data found: {storage_name}");
                    } else {
                        warn!(
                            "Duplicate non-canonical bundle data found: {storage_name} != {}",
                            existing.storage_name()
                        );
                    }
                    metrics::counter!("bpa.restart.duplicate").increment(1);
                    return;
                }
                true
            } else {
                false
            };

        let data = Bytes::from(new_data);
        let Ok(new_storage_name) = self.store.save_data(&data).await else {
            return;
        };

        let bundle = Bundle {
            metadata: BundleMetadata {
                read_only: ReadOnlyMetadata {
                    received_at: file_time,
                    ..Default::default()
                },
                ..Default::default()
            },
            bundle,
            state: Stored {
                storage_name: new_storage_name,
            },
        };

        if !exists {
            if !self.store.insert_metadata(&bundle).await.unwrap_or(false) {
                let _ = self.store.delete_data(bundle.storage_name()).await;
                metrics::counter!("bpa.restart.orphan_tombstoned").increment(1);
                return;
            }

            let reason = if report_unsupported {
                ReasonCode::BlockUnsupported
            } else {
                ReasonCode::NoAdditionalInformation
            };
            self.dispatcher
                .report_bundle_reception(&bundle, reason)
                .await;
        } else {
            let _ = self.store.update_metadata(&bundle).await;
        }

        self.dispatcher.ingest_bundle(bundle, data).await;
        metrics::counter!("bpa.restart.orphan").increment(1);
    }

    /// Discard a bundle that was previously accepted but is now invalid.
    async fn discard_invalid(
        &self,
        bundle: Bpv7Bundle,
        reason: ReasonCode,
        error: impl core::fmt::Display,
        storage_name: Arc<str>,
        file_time: time::OffsetDateTime,
    ) {
        warn!("Invalid bundle found: {storage_name}, {error}");

        let _ = self.store.delete_data(&storage_name).await;
        metrics::counter!("bpa.restart.junk").increment(1);

        if let Some(existing) = self.store.confirm_exists(&bundle.id).await.ok().flatten() {
            if existing.storage_name() != &storage_name {
                if existing.storage_name().is_empty() {
                    warn!("Invalid copy of processed bundle data found: {storage_name}");
                } else {
                    warn!(
                        "Duplicate invalid bundle data found: {storage_name} != {}",
                        existing.storage_name()
                    );
                }
            } else {
                let bundle = Bundle {
                    metadata: BundleMetadata {
                        read_only: ReadOnlyMetadata {
                            received_at: file_time,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    bundle,
                    state: Stored { storage_name },
                };

                metrics::counter!("bpa.bundle.dropped", "reason" => reason_label(&reason))
                    .increment(1);
                self.dispatcher
                    .report_bundle_deletion(&bundle, reason)
                    .await;
                let _ = self.store.tombstone_metadata(&bundle.bundle.id).await;
            }
        }
    }

    /// Discard unparseable data from storage.
    async fn discard_junk(&self, storage_name: &str, error: impl core::fmt::Display) {
        warn!("Junk data found: {storage_name}, {error}");
        let _ = self.store.delete_data(storage_name).await;
        metrics::counter!("bpa.restart.junk").increment(1);
    }

    /// Delete duplicate bundle data and log the reason.
    async fn delete_duplicate(&self, storage_name: &str, existing_name: &Arc<str>) {
        if existing_name.is_empty() {
            warn!("Duplicate processed bundle data found: {storage_name}");
        } else {
            warn!("Duplicate valid bundle data found: {storage_name} != {existing_name}",);
        }
        let _ = self.store.delete_data(storage_name).await;
        metrics::counter!("bpa.restart.duplicate").increment(1);
    }
}
