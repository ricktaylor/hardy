use bytes::Bytes;
use hardy_bpv7::bundle::RewrittenBundle;
use hardy_bpv7::status_report::ReasonCode;
use time::OffsetDateTime;
use tracing::warn;

#[cfg(feature = "tracing")]
use crate::instrument;

use super::Dispatcher;
use crate::Arc;
use crate::bundle::{Bundle, BundleMetadata, BundleStatus, ReadOnlyMetadata};
use crate::storage::RestartResult;

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub(crate) async fn restart_bundle(
        self: &Arc<Self>,
        storage_name: Arc<str>,
        file_time: OffsetDateTime,
    ) -> RestartResult {
        let Some(data) = self.store.load_data(&storage_name).await else {
            // Data has gone while we were restarting
            return RestartResult::Missing;
        };

        match RewrittenBundle::parse(&data, self.key_provider()) {
            Ok(RewrittenBundle::Valid {
                bundle,
                report_unsupported,
            }) => {
                if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
                    if metadata.storage_name.as_ref() != Some(&storage_name) {
                        if metadata.storage_name.is_none() {
                            warn!("Duplicate processed bundle data found: {storage_name}");
                        } else {
                            warn!(
                                "Duplicate valid bundle data found: {storage_name} != {:?}",
                                metadata.storage_name.as_ref()
                            );
                        }

                        self.store.delete_data(&storage_name).await;
                        RestartResult::Duplicate
                    } else {
                        match &metadata.status {
                            BundleStatus::New => {
                                let bundle = Bundle { metadata, bundle };
                                self.ingest_bundle(bundle, data).await;
                                RestartResult::Valid
                            }
                            BundleStatus::Dispatching => {
                                let bundle = Bundle { metadata, bundle };
                                self.dispatch_bundle(bundle).await;
                                RestartResult::Valid
                            }
                            BundleStatus::WaitingForService { service: _ } => {
                                let bundle = Bundle { metadata, bundle };
                                self.ingest_bundle(bundle, data).await;
                                RestartResult::Valid
                            }
                            // Other statuses are handled by their respective recovery mechanisms:
                            // - ForwardPending: CLA peer queue recovery
                            // - Waiting: poll_waiting recovery
                            // - AduFragment: fragment reassembly polling
                            _ => RestartResult::Valid,
                        }
                    }
                } else {
                    let bundle = Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(storage_name),
                            read_only: ReadOnlyMetadata {
                                received_at: file_time,
                                ..Default::default()
                            },
                            ..Default::default()
                        },
                        bundle,
                    };

                    self.store.insert_metadata(&bundle).await;

                    self.report_bundle_reception(
                        &bundle,
                        if report_unsupported {
                            ReasonCode::BlockUnsupported
                        } else {
                            ReasonCode::NoAdditionalInformation
                        },
                    )
                    .await;

                    self.ingest_bundle(bundle, data).await;

                    RestartResult::Orphan
                }
            }
            Ok(RewrittenBundle::Rewritten {
                bundle,
                new_data,
                report_unsupported,
                non_canonical: _,
            }) => {
                warn!("Bundle in non-canonical format found: {storage_name}");

                let exists = if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
                    if metadata.storage_name.as_ref() != Some(&storage_name) {
                        if metadata.storage_name.is_none() {
                            warn!(
                                "Non-canonical copy of processed bundle data found: {storage_name}"
                            );
                        } else {
                            warn!(
                                "Duplicate non-canonical bundle data found: {storage_name} != {:?}",
                                metadata.storage_name.as_ref()
                            );
                        }

                        self.store.delete_data(&storage_name).await;
                        return RestartResult::Duplicate;
                    }
                    true
                } else {
                    false
                };

                let data = Bytes::from(new_data);
                let new_storage_name = self.store.save_data(&data).await;

                self.store.delete_data(&storage_name).await;

                let bundle = Bundle {
                    metadata: BundleMetadata {
                        storage_name: Some(new_storage_name),
                        read_only: ReadOnlyMetadata {
                            received_at: file_time,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    bundle,
                };

                if !exists {
                    self.store.insert_metadata(&bundle).await;

                    self.report_bundle_reception(
                        &bundle,
                        if report_unsupported {
                            ReasonCode::BlockUnsupported
                        } else {
                            ReasonCode::NoAdditionalInformation
                        },
                    )
                    .await;
                } else {
                    self.store.update_metadata(&bundle).await;
                }

                self.ingest_bundle(bundle, data).await;
                RestartResult::Orphan
            }
            Ok(RewrittenBundle::Invalid {
                bundle,
                reason,
                error,
            }) => {
                warn!("Invalid bundle found: {storage_name}, {error}");

                let exists = if let Some(metadata) = self.store.confirm_exists(&bundle.id).await {
                    if metadata.storage_name.as_ref() != Some(&storage_name) {
                        if metadata.storage_name.is_none() {
                            warn!("Invalid copy of processed bundle data found: {storage_name}");
                        } else {
                            warn!(
                                "Duplicate invalid bundle data found: {storage_name} != {:?}",
                                metadata.storage_name.as_ref()
                            );
                        }

                        self.store.delete_data(&storage_name).await;
                        return RestartResult::Duplicate;
                    }
                    true
                } else {
                    false
                };

                self.store.delete_data(&storage_name).await;

                let bundle = Bundle {
                    metadata: BundleMetadata {
                        read_only: ReadOnlyMetadata {
                            received_at: file_time,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    bundle,
                };

                if !exists {
                    self.store.insert_metadata(&bundle).await;

                    self.report_bundle_reception(&bundle, ReasonCode::NoAdditionalInformation)
                        .await;
                } else {
                    self.store.update_metadata(&bundle).await;
                }

                self.drop_bundle(bundle, Some(reason)).await;
                RestartResult::Orphan
            }
            Err(e) => {
                warn!("Junk data found: {storage_name}, {e}");

                // TODO:  This is where we can wrap the damaged bundle in a "Junk Bundle Payload" and forward it to a 'lost+found' endpoint.  For now we just drop it.

                self.store.delete_data(&storage_name).await;
                RestartResult::Junk
            }
        }
    }
}
