use super::*;
use hardy_bpv7::status_report::ReasonCode;

pub(super) enum DispatchResult {
    Gone,
    Drop(Option<ReasonCode>),
    Forward(u32),
    Wait,
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
            match hardy_bpv7::bundle::ValidBundle::parse(&data, self.key_store())? {
                hardy_bpv7::bundle::ValidBundle::Valid(bundle, report_unsupported) => (
                    bundle::Bundle {
                        metadata: BundleMetadata {
                            storage_name: Some(self.store.save_data(data).await),
                            received_at,
                            ..Default::default()
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
                                storage_name: Some(self.store.save_data(data.into()).await),
                                received_at,
                                non_canonical,
                                ..Default::default()
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
                                received_at,
                                ..Default::default()
                            },
                            bundle,
                        },
                        Some(reason),
                        false,
                    )
                }
            };

        if !self.store.insert_metadata(&bundle).await {
            // Bundle with matching id already exists in the metadata store

            // TODO: There may be custody transfer signalling that needs to happen here

            // Drop the stored data and do not process further
            if let Some(storage_name) = &bundle.metadata.storage_name {
                self.store.delete_data(storage_name).await;
            }
            return Ok(());
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

        if reason.is_some() {
            // Not valid, drop it
            self.drop_bundle(bundle, reason).await;
        } else {
            // Now process the bundle
            self.dispatch_bundle(bundle).await;
        }
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub async fn dispatch_bundle(self: &Arc<Self>, mut bundle: bundle::Bundle) {
        if let Some(u) = bundle.bundle.flags.unrecognised {
            trace!("Bundle primary block has unrecognised flag bits set: {u:#x}");
        }

        // We loop here because of reassembly
        loop {
            // Check some basic semantic validity, lifetime first
            if bundle.has_expired() {
                trace!("Bundle lifetime has expired");
                return self
                    .drop_bundle(bundle, Some(ReasonCode::LifetimeExpired))
                    .await;
            }

            // Check hop count exceeded
            if let Some(hop_info) = bundle.bundle.hop_count.as_ref()
                && hop_info.count >= hop_info.limit
            {
                trace!("Bundle hop-limit {} exceeded", hop_info.limit);
                return self
                    .drop_bundle(bundle, Some(ReasonCode::HopLimitExceeded))
                    .await;
            }

            // TODO: Pluggable ingress filters!

            // Check for reassembly
            if bundle.bundle.id.fragment_info.is_some() {
                let reassemble = false;

                // TODO: Pluggable reassembly filters

                if reassemble
                    || match &bundle.bundle.id.source {
                        Eid::LocalNode { .. } => true,
                        Eid::LegacyIpn {
                            allocator_id,
                            node_number,
                            ..
                        }
                        | Eid::Ipn {
                            allocator_id,
                            node_number,
                            ..
                        } => {
                            if let Some((a, n)) = &self.node_ids.ipn {
                                a == allocator_id && n == node_number
                            } else {
                                false
                            }
                        }
                        Eid::Dtn { node_name, .. } => {
                            if let Some(n) = &self.node_ids.dtn {
                                node_name == n
                            } else {
                                false
                            }
                        }
                        _ => false,
                    }
                {
                    let Some((mut new_bundle, data)) = self.store.adu_reassemble(bundle).await
                    else {
                        // Nothing more to do, the store has done the work
                        return;
                    };

                    // Reparse the reconstituted bundle, for sanity
                    match hardy_bpv7::bundle::ValidBundle::parse(&data, self.key_store()) {
                        Ok(hardy_bpv7::bundle::ValidBundle::Valid(..)) => {}
                        Ok(hardy_bpv7::bundle::ValidBundle::Rewritten(
                            bundle,
                            data,
                            _,
                            non_canonical,
                        )) => {
                            trace!("Reassembled bundle has been rewritten");

                            // Update the metadata
                            new_bundle.metadata.non_canonical = non_canonical;
                            let old_storage_name = new_bundle
                                .metadata
                                .storage_name
                                .replace(self.store.save_data(data.into()).await)
                                .unwrap();
                            new_bundle.bundle = bundle;
                            self.store.update_metadata(&new_bundle).await;

                            // And drop the original bundle data
                            self.store.delete_data(&old_storage_name).await;
                        }
                        Ok(hardy_bpv7::bundle::ValidBundle::Invalid(_, _, e)) | Err(e) => {
                            // Reconstituted bundle is garbage
                            trace!("Reassembled bundle is invalid: {e}");
                            return self.delete_bundle(new_bundle).await;
                        }
                    }

                    // Dispatch the reassembled bundle
                    bundle = new_bundle;
                    continue;
                }
            }

            // By the time we get here, we've reassembled or the bundle isn't an ADU fragment
            break;
        }

        // Now process the bundle
        match self.process_bundle(&mut bundle).await {
            DispatchResult::Gone => {}
            DispatchResult::Drop(reason_code) => self.drop_bundle(bundle, reason_code).await,
            DispatchResult::Forward(peer) => {
                self.cla_registry.forward(peer, bundle).await;
            }
            DispatchResult::Wait => {
                self.store.watch_bundle(bundle).await;
            }
            DispatchResult::Delivered => {
                self.report_bundle_delivery(&bundle).await;
                self.drop_bundle(bundle, None).await;
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    pub(super) async fn process_bundle(
        self: &Arc<Self>,
        bundle: &mut bundle::Bundle,
    ) -> DispatchResult {
        // Perform RIB lookup
        match self.rib.find(bundle).await {
            Some(rib::FindResult::Drop(reason)) => {
                trace!("Bundle is black-holed");
                DispatchResult::Drop(reason)
            }
            Some(rib::FindResult::AdminEndpoint) => {
                // The bundle is for the Administrative Endpoint
                self.administrative_bundle(bundle).await
            }
            Some(rib::FindResult::Deliver(Some(service))) => {
                // TODO:  This needs to move to a storage::channel

                // Bundle is for a local service
                self.deliver_bundle(service, bundle)
                    .await
                    .trace_expect("Failed to deliver bundle")
            }
            Some(rib::FindResult::Forward(peer)) => {
                trace!("Queuing bundle for forwarding to CLA peer {peer}");
                DispatchResult::Forward(peer)
            }
            _ => {
                // Just wait
                trace!("Delaying bundle until a forwarding opportunity arises");

                if bundle.metadata.status != BundleStatus::Waiting {
                    bundle.metadata.status = BundleStatus::Waiting;
                    self.store.update_metadata(bundle).await;
                }
                DispatchResult::Wait
            }
        }
    }

    pub async fn poll_waiting(self: &Arc<Self>, cancel_token: tokio_util::sync::CancellationToken) {
        let dispatcher = self.clone();
        let (tx, rx) = flume::bounded::<bundle::Bundle>(self.poll_channel_depth);
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
                                match dispatcher.process_bundle(&mut bundle).await {
                                    DispatchResult::Drop(reason_code) => {
                                        dispatcher.drop_bundle(bundle, reason_code).await;
                                    }
                                    DispatchResult::Forward(peer) => {
                                        dispatcher.cla_registry.forward(peer, bundle).await;
                                    }
                                    DispatchResult::Wait | DispatchResult::Gone => {}
                                    DispatchResult::Delivered => {
                                        dispatcher.report_bundle_delivery(&bundle).await;
                                        dispatcher.drop_bundle(bundle, None).await;
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
            while task_set.join_next().await.is_some() {}
        };

        #[cfg(feature = "tracing")]
        let task = {
            let span = tracing::trace_span!("parent: None", "poll_waiting_reader");
            span.follows_from(tracing::Span::current());
            task.instrument(span)
        };

        let h = tokio::spawn(task);

        self.store.poll_waiting(tx).await;

        _ = h.await;
    }
}
