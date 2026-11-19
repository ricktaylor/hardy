use super::*;
use hardy_bpv7::status_report::AdministrativeRecord;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    pub(super) async fn administrative_bundle(&self, bundle: bundle::Bundle) {
        metrics::counter!("bpa.admin_record.received").increment(1);

        if !bundle.primary().flags.is_admin_record {
            debug!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            metrics::counter!("bpa.admin_record.unknown").increment(1);
            return self
                .drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                .await;
        }

        let Some((mut bundle, data)) = self.load_data_or_drop(bundle).await else {
            return;
        };

        // `KeyProvider` needs a `&Bundle`, so the re-parse, the key lookup
        // and the block read happen inline. A match expression scopes them,
        // so that the parse `OperationSet`s, which hold `Rc`s and are
        // therefore `!Send`, are dropped at the arm boundary, before any
        // `.await` in this async fn. The parse consumes `data` and returns
        // the authoritative buffer, the streaming path having concatenated
        // its pushes, and the payload becomes an owned `Bytes` before the
        // arm ends.
        let payload_result = match hardy_bpv7::parse::parse(data) {
            Ok(hardy_bpv7::parse::Parsed {
                data: buf,
                bundle: raw,
                bcbs: bcb_ops,
                ..
            }) => {
                let key_source = self.key_source(&raw, &buf);
                match hardy_bpv7::bpsec::DecryptingReader::new(
                    &raw.blocks,
                    &buf,
                    &bcb_ops,
                    &*key_source,
                )
                .into_block_data(1)
                .and_then(|p| p.ok_or(hardy_bpv7::Error::Altered))
                {
                    Ok(hardy_bpv7::block::Payload::Borrowed(s)) => Ok(buf.slice_ref(s)),
                    Ok(hardy_bpv7::block::Payload::Decrypted(d)) => Ok(Bytes::from_owner(d)),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };

        let data = match payload_result {
            Err(hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::NoKey)) => {
                // TODO: decide what an undecryptable payload deserves.
                // Parking in `Waiting` retries the decrypt on each routing
                // event, since a key can arrive later, and the reaper still
                // expires it. A lost swap means another resolver got there
                // first.
                if self
                    .store
                    .swap_status(&mut bundle, &bundle::BundleStatus::Waiting)
                    .await
                {
                    return self.store.watch_bundle(bundle).await;
                }
                return;
            }
            Err(e) => {
                debug!("Received an invalid administrative record: {e}");
                return self
                    .drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                    .await;
            }
            Ok(data) => data,
        };

        // The administrative record is the whole payload, so `parse_exact`
        // rejects the trailing bytes `parse` would allow, which are a
        // smuggling vector.
        match hardy_cbor::decode::parse_exact(data.as_ref()) {
            Err(e) => {
                debug!("Failed to parse administrative record: {e}");
                metrics::counter!("bpa.admin_record.unknown").increment(1);
                self.drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                    .await
            }
            Ok(AdministrativeRecord::BundleStatusReport(report)) => {
                debug!("Received administrative record: {report:?}");

                if report.received.is_some() {
                    metrics::counter!("bpa.status_report.received", "type" => "reception")
                        .increment(1);
                }
                if report.forwarded.is_some() {
                    metrics::counter!("bpa.status_report.received", "type" => "forwarding")
                        .increment(1);
                }
                if report.delivered.is_some() {
                    metrics::counter!("bpa.status_report.received", "type" => "delivery")
                        .increment(1);
                }
                if report.deleted.is_some() {
                    metrics::counter!("bpa.status_report.received", "type" => "deletion")
                        .increment(1);
                }

                if let Some(record) = self.rib.find_service(&report.bundle_id.source) {
                    if let Some(assertion) = report.received {
                        record
                            .kind
                            .report_status(
                                &report.bundle_id,
                                &bundle.id().source,
                                service::StatusNotify::Received,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.forwarded {
                        record
                            .kind
                            .report_status(
                                &report.bundle_id,
                                &bundle.id().source,
                                service::StatusNotify::Forwarded,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.delivered {
                        record
                            .kind
                            .report_status(
                                &report.bundle_id,
                                &bundle.id().source,
                                service::StatusNotify::Delivered,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.deleted {
                        record
                            .kind
                            .report_status(
                                &report.bundle_id,
                                &bundle.id().source,
                                service::StatusNotify::Deleted,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }

                    // A status report calls for no counter and no report of
                    // its own.
                    self.delete_bundle(bundle).await;
                } else {
                    // Park under the canonical registration EID, the key
                    // `poll_service_waiting` matches on registration, with
                    // the same foreign-EID fallback as `deliver_bundle`: a
                    // report about another node's bundle matches no local
                    // registration either way.
                    let service = self
                        .node_ids
                        .local_service_eid(&report.bundle_id.source)
                        .unwrap_or_else(|| report.bundle_id.source.clone());
                    let desired = bundle::BundleStatus::WaitingForService { service };

                    // The swap is conditional because the reaper can resolve
                    // the bundle at any await, and the park must not
                    // resurrect a tombstone.
                    if self.store.swap_status(&mut bundle, &desired).await {
                        self.store.watch_bundle(bundle).await;
                    }
                }
            }
        }
    }
}
