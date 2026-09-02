use core::time::Duration;

use alloc::borrow::Cow;

// `Bpv7Bundle` disambiguates the structural wire bundle from the BPA
// record `bundle::Bundle` used throughout this module.
use hardy_bpv7::{
    builder::Builder,
    bundle::{Bundle as Bpv7Bundle, Flags, Id},
    creation_timestamp::CreationTimestamp,
    status_report::ReasonCode,
};

use super::*;
use crate::{
    bundle::parse,
    stream::{ConcatError, Receiver, Segment, concat_stream},
};

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip(self, payload)))]
    pub async fn local_dispatch(
        self: &Arc<Self>,
        source: Eid,
        destination: Eid,
        payload: Bytes,
        lifetime: Duration,
        flags: Option<services::SendOptions>,
    ) -> Result<Id, services::Error> {
        // Build bundle and run the Originate chain before storing. The bundle
        // id is unique within this process by construction —
        // `CreationTimestamp::now` issues process-monotonic `(time,
        // sequence)` pairs — so the builder never collides with an id this
        // process issued and there is nothing to retry. `DuplicateBundle`
        // surfaces a duplicate already in the store (in practice a
        // pre-restart bundle after a backward clock step, made vanishingly
        // unlikely by the nanosecond-seeded sequence floor) — and only
        // that: a metadata-storage failure aborts inside `Store::store`.
        let mut builder = Builder::new(source, destination.clone()).with_lifetime(lifetime);

        // Set flags
        if let Some(flags) = &flags {
            builder = builder.with_flags(Flags {
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
            .with_payload(Cow::Borrowed(&payload))
            .build(CreationTimestamp::now())
            .map_err(|e| services::Error::Internal(e.into()))?;

        let data = Bytes::from(data);
        let extensions = parse::extract_from_built(&bundle, &data)
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
        stream: &mut dyn Receiver<Segment>,
    ) -> Result<Id, services::Error> {
        let data = concat_stream(stream, self.max_bundle_size_mem())
            .await
            .map_err(|e| match e {
                ConcatError::Cancelled => services::Error::StreamCancelled,
                ConcatError::TooLarge { size, max } => services::Error::PayloadTooLarge {
                    size: size as u64,
                    max: max as u64,
                },
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
    ) -> Result<Id, services::Error> {
        // Parse + validate the bundle (security boundary — can't trust
        // service-provided bytes). Non-canonical input is rejected, not rewritten;
        // the bytes are stored and forwarded as received. As the origin we must be
        // able to process HopCount / unclocked BundleAge, so an undecryptable one
        // is fatal.
        let validated = parse::parse_validate_with_provider(data.clone(), self.key_provider())?;
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
        bundle: Bpv7Bundle,
        extensions: bundle::ExtensionFields,
        data: Bytes,
    ) -> Result<Id, services::Error> {
        // Wrap in bundle::Bundle with Dispatching status so that restart
        // recovery skips the Ingress chain (originated bundles only run the
        // Originate chain, never the Ingress chain).
        let bundle = bundle::Bundle {
            bpv7: bundle,
            metadata: bundle::BundleMetadata::originated().with_extensions(extensions),
            status: bundle::BundleStatus::Dispatching,
        };

        // Inline lifetime/hop admission check for the raw-bytes path: the
        // Builder cannot produce an expired or hop-exhausted bundle, but
        // parse-validated service bytes can — the origination-side twin of
        // ingress's pre-drain gate. Nothing is stored yet, so a rejection
        // is purely an error to the caller.
        if bundle.has_expired() {
            return Err(services::Error::Dropped(Some(ReasonCode::LifetimeExpired)));
        }
        if let Some(hop_info) = &bundle.metadata.extensions.hop_count
            && hop_info.count > u64::from(hop_info.limit.get())
        {
            return Err(services::Error::Dropped(Some(ReasonCode::HopLimitExceeded)));
        }

        // Run the Originate filter hook (pure in-memory, pre-store); a Drop
        // returns its reason to the originating service.
        let (mut bundle, data) = match self
            .filter_engine
            .exec(filter::Hook::Originate, bundle, data, self.key_provider())
            .await
        {
            Ok(filter::ExecResult::Continue(_, bundle, data)) => (bundle, data),
            Ok(filter::ExecResult::Drop(_, reason)) => {
                return Err(services::Error::Dropped(reason));
            }
            Err(e) => {
                error!("Originate filter execution failed: {e}");
                return Err(services::Error::Internal(e.into()));
            }
        };

        // Now store (single persist operation, preserves filter-applied
        // metadata). False means duplicate and nothing else: a backend
        // failure aborts inside store().
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
}
