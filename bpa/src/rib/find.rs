use super::*;
use core::hash::BuildHasher;

#[derive(Debug)]
enum InternalFindResult<'a> {
    AdminEndpoint,
    Deliver(Option<Arc<services::registry::Service>>), // Deliver to local service
    Forward(Vec<(u32, &'a Eid)>),                      // sorted peer -> next_hop pairs
    Drop(Option<ReasonCode>),                          // Drop with reason code
    Reflect,                                           // Reflect
}

/// Insert into a sorted vec, maintaining sort order by peer id. Skips duplicates.
fn sorted_insert<'a>(peers: &mut Vec<(u32, &'a Eid)>, peer: u32, next_hop: &'a Eid) {
    if let Err(idx) = peers.binary_search_by_key(&peer, |&(p, _)| p) {
        peers.insert(idx, (peer, next_hop));
    }
}

impl Rib {
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub fn find(&self, bundle: &mut bundle::Bundle) -> Option<FindResult> {
        let inner = self.inner.read();

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
            return map_result(
                result,
                &self.ecmp_hash_state,
                &bundle.bundle,
                &mut bundle.metadata,
            );
        };

        // Reflect: return the bundle via the previous forwarding node,
        // falling back to the bundle source as last resort.
        let previous = bundle
            .previous_node()
            .unwrap_or_else(|| bundle.bundle.id.source.clone());

        let result = find_recurse(&inner, table, &previous, false, &mut HashSet::new())?;
        if matches!(result, InternalFindResult::Reflect) {
            // Ignore double reflection
            None
        } else {
            map_result(
                result,
                &self.ecmp_hash_state,
                &bundle.bundle,
                &mut bundle.metadata,
            )
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(to = %to)))]
    pub fn find_local(&self, to: &Eid) -> Option<FindResult> {
        let inner = self.inner.read();

        let result = find_local_inner(&inner, to)?;
        match result {
            InternalFindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
            InternalFindResult::Deliver(service) => Some(FindResult::Deliver(service)),
            InternalFindResult::Forward(peers) => {
                Some(FindResult::Forward(peers.first().unwrap().0))
            }
            InternalFindResult::Drop(reason) => Some(FindResult::Drop(reason)),
            InternalFindResult::Reflect => {
                unreachable!("Reflect filtered by find_local before calling map_result")
            }
        }
    }

    /// Find all peers reachable via a given EID (for queue management, next_hop not needed)
    #[cfg_attr(feature = "tracing", instrument(skip_all,fields(to = %to)))]
    pub(super) fn find_peers(&self, to: &hardy_bpv7::eid::Eid) -> Option<HashSet<u32>> {
        let inner = self.inner.read();

        // TODO: this is should be for *all* tables
        let table = &inner.routes;

        if let Some(InternalFindResult::Forward(peers)) =
            find_recurse(&inner, table, to, false, &mut HashSet::new())
        {
            Some(peers.into_iter().map(|(peer, _)| peer).collect())
        } else {
            None
        }
    }
}

fn map_result(
    result: InternalFindResult,
    ecmp_hash_state: &foldhash::quality::RandomState,
    bundle: &hardy_bpv7::bundle::Bundle,
    metadata: &mut metadata::BundleMetadata,
) -> Option<FindResult> {
    match result {
        InternalFindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
        InternalFindResult::Deliver(service) => Some(FindResult::Deliver(service)),
        InternalFindResult::Drop(reason) => Some(FindResult::Drop(reason)),
        InternalFindResult::Forward(peers) => {
            let &(peer, next_hop) = if peers.len() > 1 {
                peers
                    .get(
                        (ecmp_hash_state.hash_one((
                            &bundle.id.source,
                            &bundle.destination,
                            &metadata.writable.flow_label,
                        )) % (peers.len() as u64)) as usize,
                    )
                    .trace_expect("ECMP hash has picked an invalid entry")
            } else {
                peers.first().trace_expect("Empty CLA result from find?!?")
            };

            // Set the next-hop for Egress filters
            metadata.read_only.next_hop = Some(next_hop.clone());

            Some(FindResult::Forward(peer))
        }
        InternalFindResult::Reflect => {
            unreachable!("Reflect filtered by find() before calling map_result")
        }
    }
}

#[cfg_attr(feature = "tracing", instrument(skip_all,fields(to = %to)))]
fn find_local_inner<'a>(inner: &'a RibInner, to: &'a Eid) -> Option<InternalFindResult<'a>> {
    let mut peers: Option<Vec<(u32, &'a Eid)>> = None;

    // Iterate through all local patterns and find matches
    for (pattern, actions) in &inner.locals.actions {
        if pattern.matches(to) {
            for action in actions {
                match &action {
                    local::Action::AdminEndpoint => {
                        debug!("Deliver to Admin Endpoint");
                        return Some(InternalFindResult::AdminEndpoint);
                    }
                    local::Action::Local(service) => {
                        debug!("Deliver to Service {}", service.service_id);
                        return Some(InternalFindResult::Deliver(Some(service.clone())));
                    }
                    local::Action::Forward(peer) => {
                        // The 'to' Eid is the next-hop for all peers found here
                        if let Some(peers) = &mut peers {
                            sorted_insert(peers, *peer, to);
                        } else {
                            peers = Some(vec![(*peer, to)]);
                        }
                    }
                }
            }
        }
    }

    if let Some(ref peers) = peers {
        debug!(
            "Forward to CLA peer{} {}",
            if peers.len() == 1 { "" } else { "s:" },
            peers.iter().fold(String::new(), |acc, (k, v)| {
                if acc.is_empty() {
                    format!("{k} ({v})")
                } else {
                    format!("{acc}, {k} ({v})")
                }
            })
        );
    } else {
        debug!("No CLA peers found");
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
) -> Option<InternalFindResult<'a>> {
    debug!("Looking for route for {to}");

    // Always check locals first
    let mut result = find_local_inner(inner, to);
    if result.is_some() {
        return result;
    }

    'priority: for entries in table.values() {
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
                                let InternalFindResult::Forward(sub_peers) = sub_result else {
                                    // If we find a non-forward, then get out
                                    return Some(sub_result);
                                };

                                // The 'via' Eid is the next-hop for all peers found through this path
                                // Append peers to the running vec, maintaining sort order
                                if let Some(InternalFindResult::Forward(peers)) = &mut result {
                                    for (peer, _) in sub_peers {
                                        sorted_insert(peers, peer, via);
                                    }
                                } else {
                                    let mut peers: Vec<(u32, &'a Eid)> = sub_peers
                                        .into_iter()
                                        .map(|(peer, _)| (peer, via))
                                        .collect();
                                    peers.sort_unstable_by_key(|&(peer, _)| peer);
                                    result = Some(InternalFindResult::Forward(peers));
                                }
                            } else {
                                // TODO: Kick off a resolver lookup for `via`
                            }
                        }
                    }
                }
                break 'priority; // Exit both loops - no need to check lower priority entries
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
    use super::super::tests::{add_local_forward, add_route, make_rib};
    use super::*;

    fn make_bundle(destination: &str) -> bundle::Bundle {
        bundle::Bundle {
            bundle: hardy_bpv7::bundle::Bundle {
                id: hardy_bpv7::bundle::Id {
                    source: "ipn:0.99.1".parse().unwrap(),
                    timestamp: hardy_bpv7::creation_timestamp::CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: destination.parse().unwrap(),
                report_to: Default::default(),
                lifetime: core::time::Duration::from_secs(3600),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        }
    }

    fn ipn_node(n: u32) -> hardy_bpv7::eid::NodeId {
        hardy_bpv7::eid::NodeId::Ipn(hardy_bpv7::eid::IpnNodeId {
            allocator_id: 0,
            node_number: n,
        })
    }

    #[test]
    fn test_exact_match() {
        let rib = make_rib();

        // Add a local forward peer for ipn:0.2.*
        add_local_forward(&rib, ipn_node(2), 42);

        // Lookup for an EID under that node
        let result = rib.find_local(&"ipn:0.2.1".parse().unwrap());
        assert!(matches!(result, Some(FindResult::Forward(42))));
    }

    #[test]
    fn test_default_route() {
        let rib = make_rib();

        // Add a catch-all Via route
        add_route(
            &rib,
            "*:**",
            "default",
            routes::Action::Via("ipn:0.10.0".parse().unwrap()),
            1000,
        );

        // Add a local forward for the gateway node
        add_local_forward(&rib, ipn_node(10), 99);

        // An unknown destination should resolve via the default route
        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(99))));
    }

    #[test]
    fn test_no_route() {
        let rib = make_rib();

        // No routes installed — unknown destination returns None (wait for route)
        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(result.is_none());
    }

    #[test]
    fn test_recursion_loop() {
        let rib = make_rib();

        // Create a routing loop: ipn:0.2.* → Via ipn:0.3.0, ipn:0.3.* → Via ipn:0.2.0
        add_route(
            &rib,
            "ipn:0.2.*",
            "loop",
            routes::Action::Via("ipn:0.3.0".parse().unwrap()),
            10,
        );
        add_route(
            &rib,
            "ipn:0.3.*",
            "loop",
            routes::Action::Via("ipn:0.2.0".parse().unwrap()),
            10,
        );

        let mut bundle = make_bundle("ipn:0.2.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(
            result,
            Some(FindResult::Drop(Some(
                ReasonCode::NoKnownRouteToDestinationFromHere
            )))
        ));
    }

    #[test]
    fn test_reflection() {
        let rib = make_rib();

        // Add a Reflect route for ipn:0.5.*
        add_route(&rib, "ipn:0.5.*", "reflect", routes::Action::Reflect, 10);

        // Add a forward peer for node 4 (the previous hop)
        add_local_forward(&rib, ipn_node(4), 77);

        // Bundle with a previous node set
        let mut bundle = make_bundle("ipn:0.5.1");
        bundle.bundle.previous_node = Some("ipn:0.4.0".parse().unwrap());

        let result = rib.find(&mut bundle);
        // Should route back to the previous node's peer
        assert!(matches!(result, Some(FindResult::Forward(77))));
    }

    #[test]
    fn test_reflection_no_double() {
        let rib = make_rib();

        // Reflect routes for both destination and previous-hop — should not
        // double-reflect (return None instead)
        add_route(&rib, "ipn:0.5.*", "r", routes::Action::Reflect, 10);
        add_route(&rib, "ipn:0.4.*", "r", routes::Action::Reflect, 10);

        let mut bundle = make_bundle("ipn:0.5.1");
        bundle.bundle.previous_node = Some("ipn:0.4.0".parse().unwrap());

        let result = rib.find(&mut bundle);
        assert!(result.is_none());
    }

    #[test]
    fn test_ecmp_hashing() {
        let rib = make_rib();

        // Two Via routes at the same priority, each resolving to a different peer
        add_route(
            &rib,
            "ipn:0.50.*",
            "ecmp_a",
            routes::Action::Via("ipn:0.10.0".parse().unwrap()),
            10,
        );
        add_route(
            &rib,
            "ipn:0.50.*",
            "ecmp_b",
            routes::Action::Via("ipn:0.11.0".parse().unwrap()),
            10,
        );

        // Add forward peers for both gateways
        add_local_forward(&rib, ipn_node(10), 10);
        add_local_forward(&rib, ipn_node(11), 11);

        // Same bundle should always hash to the same peer (deterministic)
        let mut bundle = make_bundle("ipn:0.50.1");
        let result1 = rib.find(&mut bundle);
        let peer1 = match result1 {
            Some(FindResult::Forward(p)) => p,
            other => panic!("Expected Forward, got {other:?}"),
        };

        let mut bundle2 = make_bundle("ipn:0.50.1");
        bundle2.bundle.id = bundle.bundle.id.clone();
        let result2 = rib.find(&mut bundle2);
        let peer2 = match result2 {
            Some(FindResult::Forward(p)) => p,
            other => panic!("Expected Forward, got {other:?}"),
        };

        assert_eq!(peer1, peer2, "ECMP selection must be deterministic");
        assert!(
            peer1 == 10 || peer1 == 11,
            "Peer must be one of the ECMP targets, got {peer1}"
        );
    }
}
