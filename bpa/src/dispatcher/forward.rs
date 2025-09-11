use std::hash::{Hash, Hasher};

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

        let mut next_hop = &bundle.bundle.destination;
        let mut previous = false;
        loop {
            // Perform RIB lookup
            let reflect = match self.rib.find(next_hop) {
                Err(reason) => {
                    trace!("Bundle is black-holed");
                    return Ok(ForwardResult::Drop(reason));
                }
                Ok(Some(rib::FindResult::AdminEndpoint)) => {
                    if bundle.bundle.id.fragment_info.is_some() {
                        return self.reassemble(bundle).await;
                    }

                    // The bundle is for the Administrative Endpoint
                    return self.administrative_bundle(bundle).await;
                }
                Ok(Some(rib::FindResult::Deliver(service))) => {
                    if bundle.bundle.id.fragment_info.is_some() {
                        return self.reassemble(bundle).await;
                    }

                    // Bundle is for a local service
                    return self.deliver_bundle(service, bundle).await;
                }
                Ok(Some(rib::FindResult::Forward(clas, reflect))) => {
                    if !clas.is_empty() {
                        let cla = if clas.len() > 1 {
                            // Use a hash of source+destination as the hash input
                            // TODO: Look at other flow labels here as well
                            // TODO: This is an open research topic - is this really the correct behavior with bundles?
                            let mut hasher = std::hash::DefaultHasher::default();
                            (&bundle.bundle.id.source, next_hop).hash(&mut hasher);
                            *clas
                                .get((hasher.finish() % (clas.len() as u64)) as usize)
                                .unwrap()
                        } else {
                            0
                        };

                        bundle.metadata.status = BundleStatus::ForwardPending(cla);
                        return self
                            .store
                            .update_metadata(bundle)
                            .await
                            .map(|_| ForwardResult::Keep);
                    }
                    reflect
                }
                Ok(None) => false,
            };

            // TODO: Reflect must be a proper routing rule
            if reflect && !previous {
                // Return the bundle to the source via the 'previous_node' or 'bundle.source'
                previous = true;
                next_hop = bundle
                    .bundle
                    .previous_node
                    .as_ref()
                    .unwrap_or(&bundle.bundle.id.source);

                trace!("Returning bundle to previous node {next_hop}");
            } else {
                // Just wait
                trace!("Delaying bundle until a forwarding opportunity arises");

                bundle.metadata.status = BundleStatus::NoRoute;
                return self
                    .store
                    .update_metadata(bundle)
                    .await
                    .map(|_| ForwardResult::Keep);
            }
        }
    }
}
