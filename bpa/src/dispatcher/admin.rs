use super::*;
use hardy_bpv7::status_report::AdministrativeRecord;

impl Dispatcher {
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn administrative_bundle(&self, bundle: bundle::Bundle) {
        metrics::counter!("bpa.admin_record.received").increment(1);

        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
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

        // KeyProvider needs a &Bundle; do the structural
        // re-parse + key lookup + block_data inline. Scope it as a match
        // expression so the parse OperationSets (which contain
        // `Rc<…>` and are therefore `!Send`) are dropped at the arm
        // boundary, before any `.await` in this async fn. Consume `data`
        // into the parse and work from the authoritative buffer it
        // returns (the streaming path concatenates pushes), converting
        // the payload to an owned `Bytes` before the arm ends.
        let payload_result = match hardy_bpv7::parse::parse(data) {
            Ok(hardy_bpv7::parse::Parsed {
                data: buf,
                bundle: raw,
                bcbs: bcb_ops,
                ..
            }) => {
                let key_source = self.key_source(&raw, &buf);
                match hardy_bpv7::bpsec::block_data(1, &raw.blocks, &buf, &bcb_ops, &*key_source) {
                    Ok(hardy_bpv7::block::Payload::Borrowed(s)) => Ok(buf.slice_ref(s)),
                    Ok(hardy_bpv7::block::Payload::Decrypted(d)) => Ok(Bytes::from_owner(d)),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };

        let data = match payload_result {
            Err(hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::NoKey)) => {
                // TODO: We are unable to decrypt the payload, what do we do?
                return self.store.watch_bundle(bundle).await;
            }
            Err(e) => {
                debug!("Received an invalid administrative record: {e}");
                return self
                    .drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                    .await;
            }
            Ok(data) => data,
        };

        match hardy_cbor::decode::parse(data.as_ref()) {
            Err(e) => {
                debug!("Failed to parse administrative record: {e}");
                metrics::counter!("bpa.admin_record.unknown").increment(1);
                self.drop_bundle(bundle, ReasonCode::BlockUnintelligible)
                    .await
            }
            Ok(AdministrativeRecord::BundleStatusReport(report)) => {
                debug!("Received administrative record: {report:?}");

                // Count each assertion type present in the report
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

                // Find a live service to notify
                if let Some(service) = self.rib.find_service(&report.bundle_id.source) {
                    if let Some(assertion) = report.received {
                        service
                            .on_status_notify(
                                &report.bundle_id,
                                &bundle.bundle.id.source,
                                services::StatusNotify::Received,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.forwarded {
                        service
                            .on_status_notify(
                                &report.bundle_id,
                                &bundle.bundle.id.source,
                                services::StatusNotify::Forwarded,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.delivered {
                        service
                            .on_status_notify(
                                &report.bundle_id,
                                &bundle.bundle.id.source,
                                services::StatusNotify::Delivered,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }
                    if let Some(assertion) = report.deleted {
                        service
                            .on_status_notify(
                                &report.bundle_id,
                                &bundle.bundle.id.source,
                                services::StatusNotify::Deleted,
                                report.reason,
                                assertion.0,
                            )
                            .await;
                    }

                    // Just delete the bundle, there's no required counters or reporting
                    self.delete_bundle(bundle).await;
                } else {
                    let desired = bundle::BundleStatus::WaitingForService {
                        service: report.bundle_id.source.clone(),
                    };

                    self.store.update_status(&mut bundle, &desired).await;
                    self.store.watch_bundle(bundle).await;
                }
            }
        }
    }
}
