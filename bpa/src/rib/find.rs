use super::*;
use std::hash::{Hash, Hasher};

enum InternalFindResult {
    AdminEndpoint,
    Deliver(Option<Arc<service_registry::Service>>), // Deliver to local service
    Forward(HashSet<u32>),                           // Available CLA peers for forwarding
    Drop(Option<ReasonCode>),                        // Drop with reason code
    Reflect,                                         // Reflect
}

impl Rib {
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub async fn find(&self, bundle: &bundle::Bundle) -> Option<FindResult> {
        let inner = self.inner.read().trace_expect("Failed to lock mutex");

        // TODO: this is where route table switching can occur
        let table = &inner.routes;

        let result = find_recurse(
            &inner,
            table,
            &bundle.bundle.destination,
            true,
            &mut HashSet::new(),
        )?;
        if !matches!(result, InternalFindResult::Reflect) {
            // Drop the mutex before the mapping
            return map_result(result, bundle);
        };

        // Return the bundle to the source via the 'previous_node' or 'bundle.source'
        let previous = bundle
            .bundle
            .previous_node
            .as_ref()
            .unwrap_or(&bundle.bundle.id.source);

        let result = find_recurse(&inner, table, previous, false, &mut HashSet::new())?;
        if matches!(result, InternalFindResult::Reflect) {
            // Ignore double reflection
            None
        } else {
            map_result(result, bundle)
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(to = %to)))]
    pub(super) fn find_peers(&self, to: &hardy_bpv7::eid::Eid) -> Option<HashSet<u32>> {
        let inner = self.inner.read().trace_expect("Failed to lock mutex");

        // TODO: this is should be for *all* tables
        let table = &inner.routes;

        if let Some(InternalFindResult::Forward(peers)) =
            find_recurse(&inner, table, to, false, &mut HashSet::new())
        {
            Some(peers)
        } else {
            None
        }
    }
}

fn map_result(result: InternalFindResult, bundle: &bundle::Bundle) -> Option<FindResult> {
    match result {
        InternalFindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
        InternalFindResult::Deliver(service) => Some(FindResult::Deliver(service)),
        InternalFindResult::Drop(reason) => Some(FindResult::Drop(reason)),
        InternalFindResult::Forward(peers) => {
            let peers = peers.into_iter().collect::<Vec<_>>();
            let peer = if peers.len() > 1 {
                let mut hasher = std::hash::DefaultHasher::default();
                (
                    &bundle.bundle.id.source,
                    &bundle.bundle.destination,
                    &bundle.metadata.flow_label,
                )
                    .hash(&mut hasher);

                *peers
                    .get((hasher.finish() % (peers.len() as u64)) as usize)
                    .trace_expect("ECMP hash has picked an invalid entry")
            } else {
                *peers.first().trace_expect("Empty CLA result from find?!?")
            };

            Some(FindResult::Forward(peer))
        }
        InternalFindResult::Reflect => unreachable!(),
    }
}

#[cfg_attr(feature = "tracing", instrument(skip_all,fields(to = %to)))]
fn find_local<'a>(inner: &'a RibInner, to: &'a Eid) -> Option<InternalFindResult> {
    let mut peers: Option<HashSet<u32>> = None;
    for action in inner.locals.actions.get(to).into_iter().flatten() {
        match &action {
            local::Action::AdminEndpoint => {
                return Some(InternalFindResult::AdminEndpoint);
            }
            local::Action::Local(service) => {
                return Some(InternalFindResult::Deliver(service.clone()));
            }
            local::Action::Forward(peer) => {
                if let Some(peers) = &mut peers {
                    peers.insert(*peer);
                } else {
                    peers = Some([*peer].into());
                }
            }
        }
    }
    peers.map(InternalFindResult::Forward)
}

#[cfg_attr(feature = "tracing", instrument(skip(inner, table, to, trail),fields(to = %to)))]
fn find_recurse<'a>(
    inner: &'a RibInner,
    table: &'a RouteTable,
    to: &'a Eid,
    reflect: bool,
    trail: &mut HashSet<&'a Eid>,
) -> Option<InternalFindResult> {
    debug!("Looking for route for {to}");

    // Always check locals first
    let mut result = find_local(inner, to);
    if result.is_some() {
        return result;
    }

    for entries in table.values() {
        for (pattern, actions) in entries {
            if pattern.matches(to) {
                for entry in actions {
                    match &entry.action {
                        routes::Action::Drop(reason) => {
                            // Drop trumps everything else
                            return Some(InternalFindResult::Drop(*reason));
                        }
                        routes::Action::Reflect => {
                            if reflect {
                                return Some(InternalFindResult::Reflect);
                            }
                        }
                        routes::Action::Via(via) => {
                            // Recursive lookup
                            if !trail.insert(to) {
                                warn!("Recursive route {to} found!");
                                return Some(InternalFindResult::Drop(Some(
                                    ReasonCode::NoKnownRouteToDestinationFromHere,
                                )));
                            }

                            let sub_result = find_recurse(inner, table, via, reflect, trail);

                            trail.remove(to);

                            if let Some(sub_result) = sub_result {
                                let InternalFindResult::Forward(sub_peers) = sub_result else {
                                    // If we find a non-forward, then get out
                                    return Some(sub_result);
                                };

                                // Append clas to the running set of clas
                                if let Some(InternalFindResult::Forward(peers)) = &mut result {
                                    peers.extend(sub_peers);
                                } else {
                                    result = Some(InternalFindResult::Forward(sub_peers));
                                }
                            } else {
                                // TODO: Kick off a resolver lookup for `via`
                            }
                        }
                    }
                }
                break; // No need to check lower priority entries
            }
        }
    }

    if result.is_none() && inner.locals.finals.iter().any(|e| e.matches(to)) {
        return Some(InternalFindResult::Drop(Some(
            ReasonCode::DestinationEndpointIDUnavailable,
        )));
    }
    result
}
