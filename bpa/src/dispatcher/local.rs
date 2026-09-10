use super::*;
use hardy_bpv7::{creation_timestamp::CreationTimestamp, status_report::ReasonCode};

impl Dispatcher {
    /// Run Originate filter on an in-memory bundle (not yet stored).
    /// If the filter drops the bundle, Ok(None) is returned.
    /// If the filter passes or no filter is registered, returns Ok(Some((bundle, data))).
    ///
    /// This is a pure in-memory operation - no persistence occurs here.
    /// The caller is responsible for storing the bundle after filtering.
    async fn run_originate_filter(
        &self,
        bundle: bundle::Bundle,
        data: Bytes,
    ) -> Result<Option<(bundle::Bundle, Bytes)>, crate::Error> {
        match self
            .filter_engine
            .exec(filter::Hook::Originate, bundle, data, self.key_provider())
            .await
            .inspect_err(|_e| {
                error!("Originate filter execution failed");
            })? {
            filter::ExecResult::Continue(_mutation, bundle, data) => Ok(Some((bundle, data))),
            filter::ExecResult::Drop(_bundle, _reason) => Ok(None),
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, payload)))]
    pub async fn local_dispatch(
        self: &Arc<Self>,
        source: Eid,
        destination: Eid,
        payload: Bytes,
        lifetime: core::time::Duration,
        flags: Option<services::SendOptions>,
    ) -> Result<hardy_bpv7::bundle::BundleId, services::Error> {
        // Build bundle and run Originate filter before storing. The bundle
        // id is unique within this process by construction —
        // `CreationTimestamp::now` issues process-monotonic `(time,
        // sequence)` pairs — so the builder never collides with an id this
        // process issued and there is nothing to retry. `DuplicateBundle`
        // surfaces a duplicate already in the store (in practice a
        // pre-restart bundle after a backward clock step, made vanishingly
        // unlikely by the nanosecond-seeded sequence floor) — and only
        // that: a metadata-storage failure aborts inside `Store::store`.
        let mut builder =
            hardy_bpv7::builder::Builder::new(source, destination.clone()).with_lifetime(lifetime);

        // Set flags
        if let Some(flags) = &flags {
            builder = builder.with_flags(hardy_bpv7::bundle::BundleFlags {
                do_not_fragment: flags.do_not_fragment,
                app_ack_requested: flags.request_ack,
                report_status_time: flags.report_status_time,
                receipt_report_requested: flags.notify_reception,
                forward_report_requested: flags.notify_forwarding,
                delivery_report_requested: flags.notify_delivery,
                delete_report_requested: flags.notify_deletion,
                ..Default::default()
            });

            if flags.notify_reception
                || flags.notify_forwarding
                || flags.notify_delivery
                || flags.notify_deletion
            {
                builder = builder.with_report_to(self.node_ids.get_admin_endpoint(&destination));
            }
        }

        let (bundle, data) = builder
            .with_payload(alloc::borrow::Cow::Borrowed(&payload))
            .build(CreationTimestamp::now())
            .map_err(|e| services::Error::Internal(e.into()))?;

        let data = Bytes::from(data);
        let extensions = crate::bundle::parse::extract_from_built(&bundle, &data)
            .map_err(|e| services::Error::Internal(e.into()))?;

        self.originate_bundle(bundle, extensions, data).await
    }

    /// Dispatch a bundle from a segment stream (for low-level Service trait)
    ///
    /// Accumulates the stream (bounded by `max_bundle_size`, like CLA
    /// ingress), then parses and validates the assembled bundle exactly as
    /// [`local_dispatch_raw`](Self::local_dispatch_raw) does. A producer that
    /// goes away before the final segment cancels the send: nothing has been
    /// stored, and the caller gets
    /// [`StreamCancelled`](services::Error::StreamCancelled).
    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    pub async fn local_dispatch_raw_streamed(
        self: &Arc<Self>,
        expected_source: &Eid,
        stream: &mut dyn crate::stream::Receiver<crate::stream::Segment>,
    ) -> Result<hardy_bpv7::bundle::BundleId, services::Error> {
        let data = crate::stream::concat_stream(stream, self.max_bundle_size)
            .await
            .map_err(|e| match e {
                crate::stream::ConcatError::Cancelled => services::Error::StreamCancelled,
                crate::stream::ConcatError::TooLarge { size, max } => {
                    services::Error::PayloadTooLarge {
                        size: size as u64,
                        max: max as u64,
                    }
                }
            })?;
        self.local_dispatch_raw(expected_source, data).await
    }

    /// Dispatch a bundle from raw bytes (for low-level Service trait)
    /// Parses and validates the bundle (security boundary)
    #[cfg_attr(feature = "instrument", instrument(skip(self, data)))]
    pub async fn local_dispatch_raw(
        self: &Arc<Self>,
        expected_source: &Eid,
        data: Bytes,
    ) -> Result<hardy_bpv7::bundle::BundleId, services::Error> {
        // Parse + validate the bundle (security boundary — can't trust
        // service-provided bytes). Non-canonical input is rejected, not rewritten;
        // the bytes are stored and forwarded as received. As the origin we must be
        // able to process HopCount / unclocked BundleAge, so an undecryptable one
        // is fatal.
        let validated =
            crate::bundle::parse::parse_validate_with_provider(data.clone(), self.key_provider())?;
        crate::bundle::parse::reject_undecryptable_liveness(
            &validated.nokey_ext,
            validated.bundle.primary.id.timestamp.is_clocked(),
        )?;

        // Verify source matches the registered service endpoint
        // (registration already validated that the EID belongs to our node)
        if &validated.bundle.primary.id.source != expected_source {
            return Err(services::Error::InvalidDestination(
                validated.bundle.primary.id.source.clone(),
            ));
        }

        self.originate_bundle(validated.bundle, validated.extensions, data)
            .await
    }

    async fn originate_bundle(
        self: &Arc<Self>,
        bundle: hardy_bpv7::bundle::Bundle,
        extensions: bundle::ExtensionFields,
        data: Bytes,
    ) -> Result<hardy_bpv7::bundle::BundleId, services::Error> {
        // Wrap in bundle::Bundle with Dispatching status so that restart
        // recovery skips the Ingress filter (originated bundles only run the
        // Originate filter, never the Ingress filter).
        let bundle = bundle::Bundle {
            bpv7: bundle,
            metadata: bundle::BundleMetadata::originated().with_extensions(extensions),
            status: bundle::BundleStatus::Dispatching,
        };

        // Run Originate filter (pure in-memory)
        let Some((mut bundle, data)) = self
            .run_originate_filter(bundle, data)
            .await
            .inspect_err(|e| error!("Originate filter error: {e}"))?
        else {
            return Err(services::Error::Dropped(None));
        };

        // Now store (single persist operation, preserves filter-modified
        // metadata). False means duplicate and nothing else — a backend
        // failure aborts inside store() — so the retry loop in
        // local_dispatch can never spin against a storage outage.
        if !self.store.store(&mut bundle, &data).await {
            return Err(services::Error::DuplicateBundle);
        }

        metrics::counter!("bpa.bundle.originated").increment(1);
        metrics::counter!("bpa.bundle.originated.bytes").increment(data.len() as u64);

        let bundle_id = bundle.id().clone();
        metrics::gauge!("bpa.bundle.status", "state" => crate::otel_metrics::status_label(&bundle.status)).increment(1.0);
        self.dispatch_bundle(bundle).await;
        Ok(bundle_id)
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, bundle),fields(bundle.id = %bundle.id())))]
    pub(super) async fn deliver_bundle(
        &self,
        service: Arc<services::registry::Service>,
        bundle: bundle::Bundle,
    ) {
        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // The claim key and every park in the offer use the canonical
        // registration EID stored at construction — the exact key
        // `poll_service_waiting` matches on re-registration. The bundle's
        // own destination can be a different Eid variant for the same
        // endpoint (e.g. LegacyIpn vs Ipn) and would never match.
        let service_eid = service.eid().clone();

        // Snapshot the routing table before the claim: the parks in the
        // offer re-check it to close the park-vs-poll window (see
        // park_bundle).
        let seen = self.rib.table_snapshot();

        // Delivery commits at the claim below — the reaper defers an
        // in-flight delivery — so never commence one for a bundle that has
        // already expired: resolve it as the reaper would.
        if bundle.has_expired() {
            return self.drop_bundle(bundle, ReasonCode::LifetimeExpired).await;
        }

        // Claim the bundle out of its delivery queue before offering it.
        // The claim must be a conditional swap: the delivery channel is
        // at-least-once, so a duplicate copy recovered by the storage
        // poller must lose here rather than produce a second delivery. The
        // new status also marks the point past which the delivery cannot be
        // recalled: the reaper defers it, and the unregister sweep only
        // touches the queued status.
        if !self
            .store
            .swap_status(
                &mut bundle,
                &bundle::BundleStatus::DeliveryAckPending {
                    service: service_eid.clone(),
                },
            )
            .await
        {
            debug!("Bundle already claimed for delivery or swept, skipping offer");
            return;
        }

        // Claim-to-resolution is one expression: the offer's outcome is the
        // claim's resolution.
        self.resolve_offer(
            OfferKind::Delivery,
            self.offer_to_service(service, service_eid, bundle, data, seen)
                .await,
        )
        .await
    }

    /// Offer a claimed bundle to its service. Runs strictly inside the
    /// `DeliveryAckPending` claim: every exit is an [`OfferOutcome`] the
    /// caller resolves, so the claim cannot dangle.
    async fn offer_to_service(
        &self,
        service: Arc<services::registry::Service>,
        service_eid: Eid,
        bundle: bundle::Bundle,
        data: Bytes,
        seen: routing::RibSnapshot,
    ) -> OfferOutcome {
        let bundle_id = bundle.id().clone();

        // Deliver filter hook
        let (bundle, mut data) = match self
            .filter_engine
            .exec(filter::Hook::Deliver, bundle, data, self.key_provider())
            .await
        {
            Ok(filter::ExecResult::Continue(_, bundle, data)) => (bundle, data),
            Ok(filter::ExecResult::Drop(bundle, reason)) => {
                return OfferOutcome::Dropped(bundle, reason);
            }
            Err(e) => {
                error!("Deliver filter execution failed: {e}");

                // The filter consumed the claimed bundle, so re-fetch it and
                // conditionally park it for the next registration. A
                // re-fetch that finds the bundle moved on means a sweep or
                // the reaper resolved it first.
                return match self.store.get_metadata(&bundle_id).await {
                    Some(bundle)
                        if bundle.status
                            == (bundle::BundleStatus::DeliveryAckPending {
                                service: service_eid.clone(),
                            }) =>
                    {
                        OfferOutcome::Parked(
                            bundle,
                            bundle::BundleStatus::WaitingForService {
                                service: service_eid,
                            },
                            seen,
                        )
                    }
                    _ => OfferOutcome::Lost,
                };
            }
        };

        let delivery_result = match &service.service {
            services::registry::ServiceImpl::LowLevel(svc) => {
                // Pass raw bundle bytes to low-level services: the whole
                // bundle is in hand, so it travels as a single Final segment.
                let total_len = data.len() as u64;
                svc.on_deliver(bundle.id(), bundle.expiry(), total_len, &mut data)
                    .await
            }
            services::registry::ServiceImpl::Application(app) => {
                // Extract and decrypt payload for Application.
                // KeyProvider needs a &Bundle; scope the parse
                // as a match expression so the parse OperationSets
                // (which contain `Rc<…>` and are therefore `!Send`) are
                // dropped at the arm boundary, before any `.await` in
                // this async fn. Consume `data` into the parse and work
                // from the authoritative buffer it returns (the streaming
                // path concatenates pushes), converting the payload to an
                // owned `Bytes` (zero-copy for the unencrypted case via
                // `slice_ref`) before the arm ends.
                let payload_result = match hardy_bpv7::parse::parse(data) {
                    Ok(hardy_bpv7::parse::Parsed {
                        data: buf,
                        bundle: raw,
                        bcbs: bcb_ops,
                        ..
                    }) => {
                        let key_source = self.key_source(&raw, &buf);
                        match hardy_bpv7::bpsec::block_data(
                            1,
                            &raw.blocks,
                            &buf,
                            &bcb_ops,
                            &*key_source,
                        ) {
                            Ok(hardy_bpv7::bundle::Payload::Borrowed(s)) => Ok(buf.slice_ref(s)),
                            Ok(hardy_bpv7::bundle::Payload::Decrypted(d)) => {
                                Ok(Bytes::from_owner(d))
                            }
                            Err(e) => Err(e),
                        }
                    }
                    Err(e) => Err(e),
                };

                let mut payload = match payload_result {
                    Err(hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::NoKey)) => {
                        // TODO: We are unable to decrypt the payload, what do we do?
                        // For now, park for the next registration (which may
                        // bring usable keys).
                        debug!("Failed to decrypt payload: No valid keys");
                        return OfferOutcome::Parked(
                            bundle,
                            bundle::BundleStatus::WaitingForService {
                                service: service_eid,
                            },
                            seen,
                        );
                    }
                    Err(e) => {
                        // Other decryption error - skip delivery
                        debug!("Received an invalid payload: {e}");

                        // TODO: This is where we can wrap the damaged bundle in a "Junk Bundle Payload" and forward it to a 'lost+found' endpoint.  For now we just drop it.

                        return OfferOutcome::Dropped(
                            bundle,
                            Some(ReasonCode::BlockUnintelligible),
                        );
                    }
                    Ok(payload) => payload,
                };

                // As for low-level services, the whole payload is in hand,
                // so it travels as a single Final segment.
                let total_len = payload.len() as u64;
                app.on_deliver(
                    bundle.id(),
                    bundle.expiry(),
                    bundle.primary().flags.app_ack_requested,
                    total_len,
                    &mut payload,
                )
                .await
            }
        };

        if let Err(e) = delivery_result {
            debug!("Service delivery deferred: {e}");
            // Park under the registration EID for the next registration; the
            // park re-checks the routing snapshot, so a service that
            // (re-)registered while this delivery was in flight re-dispatches
            // the bundle instead of stranding it (see park_bundle).
            return OfferOutcome::Parked(
                bundle,
                bundle::BundleStatus::WaitingForService {
                    service: service_eid,
                },
                seen,
            );
        }

        OfferOutcome::Completed(bundle)
    }
}
