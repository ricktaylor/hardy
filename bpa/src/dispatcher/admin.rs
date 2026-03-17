use super::*;
use hardy_bpv7::status_report::AdministrativeRecord;

impl Dispatcher {
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub(super) async fn administrative_bundle(&self, mut bundle: bundle::Bundle, data: Bytes) {
        // This is a bundle for an Admin Endpoint
        if !bundle.bundle.flags.is_admin_record {
            debug!(
                "Received a bundle for an administrative endpoint that isn't marked as an administrative record"
            );
            return self
                .drop_bundle(bundle, Some(ReasonCode::BlockUnintelligible))
                .await;
        }

        let payload_result = {
            let key_source = self.key_source(&bundle.bundle, &data);
            bundle.bundle.block_data(1, &data, &*key_source)
        }; // key_source dropped here, before any await

        let data = match payload_result {
            Err(hardy_bpv7::Error::InvalidBPSec(hardy_bpv7::bpsec::Error::NoKey)) => {
                // TODO: We are unable to decrypt the payload, what do we do?
                return self.store.watch_bundle(bundle).await;
            }
            Err(e) => {
                debug!("Received an invalid administrative record: {e}");
                return self
                    .drop_bundle(bundle, Some(ReasonCode::BlockUnintelligible))
                    .await;
            }
            Ok(data) => data,
        };

        match hardy_cbor::decode::parse(data.as_ref()) {
            Err(e) => {
                debug!("Failed to parse administrative record: {e}");
                self.drop_bundle(bundle, Some(ReasonCode::BlockUnintelligible))
                    .await
            }
            Ok(AdministrativeRecord::BundleStatusReport(report)) => {
                debug!("Received administrative record: {report:?}");

                // Find a live service to notify
                match self.rib.find_local(&report.bundle_id.source) {
                    Some(rib::FindResult::Deliver(Some(service))) => {
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
                        self.drop_bundle(bundle, None).await;
                    }
                    Some(_) => {
                        self.drop_bundle(bundle, None).await;
                    }
                    None => {
                        let desired = BundleStatus::WaitingForService {
                            service: report.bundle_id.source.clone(),
                        };

                        if bundle.metadata.status != desired {
                            bundle.metadata.status = desired;
                            self.store.update_metadata(&bundle).await;
                        }

                        self.store.watch_bundle(bundle).await;
                    }
                }
            }
            #[cfg(feature = "bp-arp")]
            Ok(AdministrativeRecord::BpArp(eids)) => {
                // Check if this is a Request (Probe) or Response (Ack) based on Destination EID
                // Request: Destination is ipn:!.0 (LocalNode)
                if matches!(
                    bundle.bundle.destination,
                    hardy_bpv7::eid::Eid::LocalNode(0)
                ) {
                    // A remote BPA is probing us to learn our EID.
                    // Learn the sender's EID from the bundle source and respond with a BpArpAck.
                    debug!("Received BP-ARP probe from {}", bundle.bundle.id.source);
                    let source = bundle.bundle.id.source.clone();

                    // Send a BpArpAck back so the probing BPA learns all of our EIDs (§4.3).
                    let ack_payload = hardy_cbor::encode::emit(&AdministrativeRecord::BpArp(
                        self.cla_registry.all_admin_endpoints(),
                    ))
                    .0;
                    self.dispatch_admin_bundle(ack_payload, &source).await;
                    // The remote BPA responded to our probe, revealing all of its EIDs.
                    debug!("Received BP-ARP ack from {}", bundle.bundle.id.source);

                    // Combine the bundle source EID with the payload EID list so we learn every
                    // EID the remote node advertises, even if the payload is unexpectedly empty.
                    let mut all_eids = eids;
                    let source = bundle.bundle.id.source.clone();
                    if !all_eids.contains(&source) {
                        all_eids.push(source);
                    }

                    if let Some(peer_addr) = bundle.metadata.read_only.ingress_peer_addr.clone() {
                        self.cla_registry
                            .promote_neighbour(&peer_addr, all_eids)
                            .await;
                    } else {
                        debug!("BP-ARP ack received without ingress peer address, cannot promote");
                    }
                }
                self.drop_bundle(bundle, None).await;
            }
        }
    }
}
