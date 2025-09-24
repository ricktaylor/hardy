use super::*;
use core::ops::Deref;
use hardy_bpv7::status_report::ReasonCode;

pub enum DispatchResult {
    Gone,
    Drop(Option<ReasonCode>),
    Keep,
    Delivered,
}

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn receive_bundle(self: &Arc<Self>, data: Bytes) -> cla::Result<()> {
        // Capture received_at as soon as possible
        let received_at = time::OffsetDateTime::now_utc();

        // Do a fast pre-check
        match data.first() {
            None => {
                return Err(hardy_bpv7::Error::InvalidCBOR(
                    hardy_cbor::decode::Error::NeedMoreData(1),
                )
                .into());
            }
            Some(0x06) => {
                trace!("Data looks like a BPv6 bundle");
                return Err(hardy_bpv7::Error::InvalidCBOR(
                    hardy_cbor::decode::Error::IncorrectType(
                        "BPv7 bundle".to_string(),
                        "Possible BPv6 bundle".to_string(),
                    ),
                )
                .into());
            }
            Some(0x80..=0x9F) => {}
            _ => {
                return Err(hardy_bpv7::Error::InvalidCBOR(
                    hardy_cbor::decode::Error::IncorrectType(
                        "BPv7 bundle".to_string(),
                        "Invalid CBOR".to_string(),
                    ),
                )
                .into());
            }
        }

        // Parse the bundle
        let (bundle, reason, report_unsupported) =
            match hardy_bpv7::bundle::ValidBundle::parse(&data, self.deref())? {
                hardy_bpv7::bundle::ValidBundle::Valid(bundle, report_unsupported) => (
                    bundle::Bundle {
                        metadata: BundleMetadata {
                            status: BundleStatus::Dispatching,
                            storage_name: Some(self.store.save_data(data).await?),
                            received_at,
                            non_canonical: false,
                        },
                        bundle,
                    },
                    None,
                    report_unsupported,
                ),
                hardy_bpv7::bundle::ValidBundle::Rewritten(
                    bundle,
                    data,
                    report_unsupported,
                    non_canonical,
                ) => {
                    trace!("Received bundle has been rewritten");
                    (
                        bundle::Bundle {
                            metadata: BundleMetadata {
                                status: BundleStatus::Dispatching,
                                storage_name: Some(self.store.save_data(data.into()).await?),
                                received_at,
                                non_canonical,
                            },
                            bundle,
                        },
                        None,
                        report_unsupported,
                    )
                }
                hardy_bpv7::bundle::ValidBundle::Invalid(bundle, reason, e) => {
                    trace!("Invalid bundle received: {e}");

                    // Don't bother saving the bundle data, it's garbage
                    (
                        bundle::Bundle {
                            metadata: BundleMetadata {
                                status: BundleStatus::Dispatching,
                                storage_name: None,
                                received_at,
                                non_canonical: false,
                            },
                            bundle,
                        },
                        Some(reason),
                        false,
                    )
                }
            };

        match self.store.insert_metadata(&bundle).await {
            Ok(false) => {
                // Bundle with matching id already exists in the metadata store

                // TODO: There may be custody transfer signalling that needs to happen here

                // Drop the stored data and do not process further
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    self.store.delete_data(storage_name).await?;
                }
                return Ok(());
            }
            Err(e) => {
                if let Some(storage_name) = &bundle.metadata.storage_name {
                    _ = self.store.delete_data(storage_name).await;
                }
                return Err(e.into());
            }
            _ => {}
        }

        // Report we have received the bundle
        self.report_bundle_reception(
            &bundle,
            if report_unsupported {
                ReasonCode::BlockUnsupported
            } else {
                ReasonCode::NoAdditionalInformation
            },
        )
        .await;

        // Check the bundle further
        self.process_bundle(bundle, reason)
            .await
            .map_err(Into::into)
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    pub async fn process_bundle(
        self: &Arc<Self>,
        bundle: bundle::Bundle,
        mut reason: Option<ReasonCode>,
    ) -> Result<(), Error> {
        /* Always check bundles, no matter the state, as after restarting
         * the configured filters or code may have changed, and reprocessing is desired.
         */

        if let Some(u) = bundle.bundle.flags.unrecognised {
            trace!("Bundle primary block has unrecognised flag bits set: {u:#x}");
        }

        if reason.is_none() {
            // Check some basic semantic validity, lifetime first
            if bundle.has_expired() {
                trace!("Bundle lifetime has expired");
                reason = Some(ReasonCode::LifetimeExpired);
            } else if let Some(hop_info) = bundle.bundle.hop_count.as_ref() {
                // Check hop count exceeded
                if hop_info.count >= hop_info.limit {
                    trace!("Bundle hop-limit {} exceeded", hop_info.limit);
                    reason = Some(ReasonCode::HopLimitExceeded);
                }
            }
        }

        if reason.is_some() {
            // Not valid, drop it
            return self.drop_bundle(bundle, reason).await;
        }

        // Now process the bundle
        self.dispatch_bundle(bundle).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn dispatch_bundle(
        self: &Arc<Self>,
        mut bundle: bundle::Bundle,
    ) -> Result<(), Error> {
        // Now process the bundle
        let reason_code = match self.dispatch_bundle_inner(&mut bundle).await? {
            DispatchResult::Gone => return Ok(()),
            DispatchResult::Drop(reason_code) => reason_code,
            DispatchResult::Keep => {
                self.reaper.watch_bundle(bundle).await;
                return Ok(());
            }
            DispatchResult::Delivered => {
                self.report_bundle_delivery(&bundle).await;
                None
            }
        };

        self.drop_bundle(bundle, reason_code).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub(super) async fn dispatch_bundle_inner(
        self: &Arc<Self>,
        bundle: &mut bundle::Bundle,
    ) -> Result<DispatchResult, Error> {
        // TODO: Pluggable Egress filters!

        // Perform RIB lookup
        match self.rib.find(&self.cla_registry, bundle).await {
            Some(rib::FindResult::Drop(reason)) => {
                trace!("Bundle is black-holed");
                Ok(DispatchResult::Drop(reason))
            }
            Some(rib::FindResult::AdminEndpoint) => {
                if bundle.bundle.id.fragment_info.is_some() {
                    self.reassemble(bundle).await
                } else {
                    // The bundle is for the Administrative Endpoint
                    self.administrative_bundle(bundle).await
                }
            }
            Some(rib::FindResult::Deliver(Some(service))) => {
                if bundle.bundle.id.fragment_info.is_some() {
                    self.reassemble(bundle).await
                } else {
                    // Bundle is for a local service
                    self.deliver_bundle(service, bundle).await
                }
            }
            Some(rib::FindResult::Forward { peer, queue }) => {
                trace!("Queuing bundle for forwarding to CLA peer {peer} queue {queue}");

                // Bundle is ready to forward
                if bundle.metadata.status != (BundleStatus::ForwardPending { peer, queue }) {
                    bundle.metadata.status = BundleStatus::ForwardPending { peer, queue };
                    self.store.update_metadata(bundle).await?;
                }
                Ok(DispatchResult::Keep)
            }
            _ => {
                // Just wait
                trace!("Delaying bundle until a forwarding opportunity arises");

                if bundle.metadata.status != BundleStatus::Waiting {
                    bundle.metadata.status = BundleStatus::Waiting;
                    self.store.update_metadata(bundle).await?;
                }
                Ok(DispatchResult::Keep)
            }
        }
    }

    pub async fn poll_waiting(
        self: &Arc<Self>,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) {
        // Tuning parameter
        const CHANNEL_DEPTH: usize = 16;

        let outer_cancel_token = cancel_token.child_token();
        let cancel_token = outer_cancel_token.clone();
        let dispatcher = self.clone();
        let (tx, rx) = flume::bounded::<bundle::Bundle>(CHANNEL_DEPTH);
        let task = async move {
            // We're going to spawn a bunch of tasks
            let parallelism = std::thread::available_parallelism()
                .map(Into::into)
                .unwrap_or(1);
            let mut task_set = tokio::task::JoinSet::new();
            let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism));

            loop {
                tokio::select! {
                    bundle = rx.recv_async() => {
                        let Ok(mut bundle) = bundle else {
                            break;
                        };

                        if !bundle.has_expired() {
                            let permit = semaphore.clone().acquire_owned().await.trace_expect("Failed to acquire permit");
                            let dispatcher = dispatcher.clone();
                            task_set.spawn(async move {
                                // Now process the bundle
                                match dispatcher.dispatch_bundle_inner(&mut bundle).await {
                                    Err(e) => error!("Failed to dispatch bundle: {e}"),
                                    Ok(DispatchResult::Drop(reason_code)) => {
                                        if let Err(e) = dispatcher.drop_bundle(bundle, reason_code).await {
                                            error!("Failed to drop bundle: {e}");
                                        }
                                    }
                                    Ok(DispatchResult::Keep | DispatchResult::Gone) => {}
                                    Ok(DispatchResult::Delivered) => {
                                        dispatcher.report_bundle_delivery(&bundle).await;
                                        if let Err(e) = dispatcher.drop_bundle(bundle, None).await {
                                            error!("Failed to drop bundle: {e}");
                                        }
                                    }
                                };
                                drop(permit);
                            });
                        }
                    },
                    Some(_) = task_set.join_next(), if !task_set.is_empty() => {},
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }

            // Wait for all sub-tasks to complete
            while let Some(_) = task_set.join_next().await {}
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "poll_waiting_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        if self
            .store
            .poll_waiting(tx)
            .await
            .inspect_err(|e| error!("Failed to poll store for waiting bundles: {e}"))
            .is_err()
        {
            // Cancel the reader task
            outer_cancel_token.cancel();
        }

        _ = h.await;
    }
}
