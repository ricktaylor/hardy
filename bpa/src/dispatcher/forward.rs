use super::*;

pub enum ForwardResult {
    Drop(Option<ReasonCode>),
    Keep,
    Delivered,
}

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn forward_bundle(self: &Arc<Self>, mut bundle: bundle::Bundle) -> Result<(), Error> {
        // Now process the bundle
        let reason_code = match self.forward_bundle_inner(&mut bundle).await? {
            ForwardResult::Drop(reason_code) => reason_code,
            ForwardResult::Keep => {
                self.reaper.watch_bundle(bundle).await;
                return Ok(());
            }
            ForwardResult::Delivered => {
                self.report_bundle_delivery(&bundle).await;
                None
            }
        };

        self.drop_bundle(bundle, reason_code).await
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub(super) async fn forward_bundle_inner(
        self: &Arc<Self>,
        bundle: &mut bundle::Bundle,
    ) -> Result<ForwardResult, Error> {
        // TODO: Pluggable Egress filters!

        // Perform RIB lookup
        match self.rib.find(&self.cla_registry, bundle).await {
            Some(rib::FindResult::Drop(reason)) => {
                trace!("Bundle is black-holed");
                Ok(ForwardResult::Drop(reason))
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
                trace!("Queuing bundle for forwarding to egress peer {peer} queue {queue}");

                // Bundle is ready to forward
                if bundle.metadata.status != (BundleStatus::ForwardPending { peer, queue }) {
                    bundle.metadata.status = BundleStatus::ForwardPending { peer, queue };
                    self.store.update_metadata(bundle).await?;
                }
                Ok(ForwardResult::Keep)
            }
            _ => {
                // Just wait
                trace!("Delaying bundle until a forwarding opportunity arises");

                if bundle.metadata.status != BundleStatus::Waiting {
                    bundle.metadata.status = BundleStatus::Waiting;
                    self.store.update_metadata(bundle).await?;
                }
                Ok(ForwardResult::Keep)
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
            loop {
                tokio::select! {
                    bundle = rx.recv_async() => {
                        let Ok(mut bundle) = bundle else {
                            break;
                        };

                        // TODO: Use a semaphore to rate control this

                        // Now process the bundle
                        match dispatcher.forward_bundle_inner(&mut bundle).await {
                            Err(e) => error!("Failed to reforward bundle: {e}"),
                            Ok(ForwardResult::Drop(reason_code)) => {
                                if let Err(e) = dispatcher.drop_bundle(bundle, reason_code).await {
                                    error!("Failed to drop bundle: {e}");
                                }
                            }
                            Ok(ForwardResult::Keep) => {}
                            Ok(ForwardResult::Delivered) => {
                                dispatcher.report_bundle_delivery(&bundle).await;
                                if let Err(e) = dispatcher.drop_bundle(bundle, None).await {
                                    error!("Failed to drop bundle: {e}");
                                }
                            }
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        break;
                    }
                }
            }
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
