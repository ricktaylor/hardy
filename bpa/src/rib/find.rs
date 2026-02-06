use super::*;
use std::hash::{Hash, Hasher};

#[derive(Debug)]
enum InternalFindResult<'a> {
    AdminEndpoint,
    Deliver(Option<Arc<services::registry::Service>>), // Deliver to local service
    Forward(HashMap<u32, &'a Eid>),                    // peer -> next_hop mapping
    Drop(Option<ReasonCode>),                          // Drop with reason code
    Reflect,                                           // Reflect
}

impl Rib {
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.id)))]
    pub fn find(
        &self,
        bundle: &hardy_bpv7::bundle::Bundle,
        metadata: &mut metadata::BundleMetadata,
    ) -> Option<FindResult> {
        let inner = self.inner.read().trace_expect("Failed to lock mutex");

        // TODO: this is where route table switching can occur
        let table = &inner.routes;

        let result = find_recurse(
            &inner,
            table,
            &bundle.destination,
            true,
            &mut HashSet::new(),
        )?;
        if !matches!(result, InternalFindResult::Reflect) {
            // Drop the mutex before the mapping
            return map_result(result, bundle, metadata);
        };

        // Return the bundle to the source via the 'previous_node' or 'bundle.source'
        let previous = bundle.previous_node.as_ref().unwrap_or(&bundle.id.source);

        let result = find_recurse(&inner, table, previous, false, &mut HashSet::new())?;
        if matches!(result, InternalFindResult::Reflect) {
            // Ignore double reflection
            None
        } else {
            map_result(result, bundle, metadata)
        }
    }

    /// Find all peers reachable via a given EID (for queue management, next_hop not needed)
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(to = %to)))]
    pub(super) fn find_peers(&self, to: &hardy_bpv7::eid::Eid) -> Option<HashSet<u32>> {
        let inner = self.inner.read().trace_expect("Failed to lock mutex");

        // TODO: this is should be for *all* tables
        let table = &inner.routes;

        if let Some(InternalFindResult::Forward(peer_map)) =
            find_recurse(&inner, table, to, false, &mut HashSet::new())
        {
            Some(peer_map.into_keys().collect())
        } else {
            None
        }
    }
}

fn map_result(
    result: InternalFindResult,
    bundle: &hardy_bpv7::bundle::Bundle,
    metadata: &mut metadata::BundleMetadata,
) -> Option<FindResult> {
    match result {
        InternalFindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
        InternalFindResult::Deliver(service) => Some(FindResult::Deliver(service)),
        InternalFindResult::Drop(reason) => Some(FindResult::Drop(reason)),
        InternalFindResult::Forward(peer_map) => {
            let peers: Vec<_> = peer_map.iter().collect();
            let &(&peer, &next_hop) = if peers.len() > 1 {
                let mut hasher = std::hash::DefaultHasher::default();
                (&bundle.id.source, &bundle.destination, &metadata.flow_label).hash(&mut hasher);

                peers
                    .get((hasher.finish() % (peers.len() as u64)) as usize)
                    .trace_expect("ECMP hash has picked an invalid entry")
            } else {
                peers.first().trace_expect("Empty CLA result from find?!?")
            };

            // Set the next-hop for Egress filters
            metadata.next_hop = Some(next_hop.clone());

            Some(FindResult::Forward(peer))
        }
        InternalFindResult::Reflect => unreachable!(),
    }
}

#[cfg_attr(feature = "tracing", instrument(skip_all,fields(to = %to)))]
fn find_local<'a>(inner: &'a RibInner, to: &'a Eid) -> Option<InternalFindResult<'a>> {
    let mut peer_map: Option<HashMap<u32, &'a Eid>> = None;
    for action in inner.locals.actions.get(to).into_iter().flatten() {
        match &action {
            local::Action::AdminEndpoint => {
                debug!("Deliver to Admin Endpoint");
                return Some(InternalFindResult::AdminEndpoint);
            }
            local::Action::Local(service) => {
                debug!("Deliver to Service {service:?}");
                return Some(InternalFindResult::Deliver(service.clone()));
            }
            local::Action::Forward(peer) => {
                // The 'to' Eid is the next-hop for all peers found here
                if let Some(peer_map) = &mut peer_map {
                    peer_map.insert(*peer, to);
                } else {
                    peer_map = Some([(*peer, to)].into());
                }
            }
        }
    }

    debug!("Forward to CLA peers {peer_map:?}");
    peer_map.map(InternalFindResult::Forward)
}

#[cfg_attr(feature = "tracing", instrument(skip(inner, table, to, trail),fields(to = %to)))]
fn find_recurse<'a>(
    inner: &'a RibInner,
    table: &'a RouteTable,
    to: &'a Eid,
    reflect: bool,
    trail: &mut HashSet<&'a Eid>,
) -> Option<InternalFindResult<'a>> {
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
                            debug!("Drop {reason:?}");
                            return Some(InternalFindResult::Drop(*reason));
                        }
                        routes::Action::Reflect => {
                            if reflect {
                                debug!("Reflect");
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
                                let InternalFindResult::Forward(sub_peer_map) = sub_result else {
                                    // If we find a non-forward, then get out
                                    return Some(sub_result);
                                };

                                // The 'via' Eid is the next-hop for all peers found through this path
                                let sub_peers_with_via: HashMap<u32, &'a Eid> =
                                    sub_peer_map.into_keys().map(|peer| (peer, via)).collect();

                                // Append peers to the running map
                                if let Some(InternalFindResult::Forward(peer_map)) = &mut result {
                                    peer_map.extend(sub_peers_with_via);
                                } else {
                                    result = Some(InternalFindResult::Forward(sub_peers_with_via));
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
        debug!("No route found");
        return Some(InternalFindResult::Drop(Some(
            ReasonCode::DestinationEndpointIDUnavailable,
        )));
    }

    debug!("Forward {result:?}");
    result
}

#[cfg(test)]
mod tests {
    // use super::*;

    // // TODO: Implement test for 'Exact Match' (Lookup exact EID match)
    // #[test]
    // fn test_exact_match() {
    //     todo!("Verify lookup exact EID match");
    // }

    // // TODO: Implement test for 'Longest Prefix' (Lookup with overlapping routes)
    // #[test]
    // fn test_longest_prefix() {
    //     todo!("Verify lookup with overlapping routes");
    // }

    // // TODO: Implement test for 'Default Route' (Lookup with no match but default set)
    // #[test]
    // fn test_default_route() {
    //     todo!("Verify lookup with no match but default set");
    // }

    // // TODO: Implement test for 'ECMP Hashing' (Verify deterministic peer selection (REQ-6.1.10))
    // #[test]
    // fn test_ecmp_hashing() {
    //     todo!("Verify deterministic peer selection (REQ-6.1.10)");
    // }

    // // TODO: Implement test for 'Recursion Loop' (Verify detection of routing loops)
    // #[test]
    // fn test_recursion_loop() {
    //     todo!("Verify detection of routing loops");
    // }

    // // TODO: Implement test for 'Reflection' (Verify routing to previous node (REQ-6.1.8))
    // #[test]
    // fn test_reflection() {
    //     todo!("Verify routing to previous node (REQ-6.1.8)");
    // }
}
