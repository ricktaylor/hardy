use super::*;
use core::hash::BuildHasher;
use hardy_bpv7::eid::IpnNodeId;
use route::Action;

fn pattern_match(pattern: &EidPattern, eid: &Eid, local_node: &Option<IpnNodeId>) -> bool {
    match eid {
        Eid::Ipn {
            fqnn,
            service_number,
        }
        | Eid::LegacyIpn {
            fqnn,
            service_number,
        } => {
            pattern.matches(eid)
                || local_node.is_some_and(|node_id| {
                    *fqnn == node_id && pattern.matches(&Eid::LocalNode(*service_number))
                })
        }
        _ => pattern.matches(eid),
    }
}

#[derive(Debug)]
enum InternalFindResult<'a> {
    AdminEndpoint,
    Deliver(Arc<services::registry::Service>), // Deliver to local service
    Forward(Vec<(u32, &'a Eid)>),              // sorted peer -> next_hop pairs
    Drop(Option<ReasonCode>),                  // Drop with reason code
    Reflect,                                   // Reflect
}

/// Insert into a sorted vec, maintaining sort order by peer id. Skips duplicates.
fn sorted_insert<'a>(peers: &mut Vec<(u32, &'a Eid)>, peer: u32, next_hop: &'a Eid) {
    if let Err(idx) = peers.binary_search_by_key(&peer, |&(p, _)| p) {
        peers.insert(idx, (peer, next_hop));
    }
}

impl Rib {
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub fn find(&self, bundle: &mut bundle::Bundle) -> Option<FindResult> {
        let inner = self.inner.read();

        // TODO: this is where route table switching can occur
        let table = &inner.routes;

        let result = find_recurse(
            table,
            &bundle.bundle.destination,
            true,
            &mut HashSet::new(),
            &self.node_ids.ipn,
        )?;
        if !matches!(result, InternalFindResult::Reflect) {
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

        let result = find_recurse(
            table,
            &previous,
            false,
            &mut HashSet::new(),
            &self.node_ids.ipn,
        )?;
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

    /// Find all peers reachable via a given EID (for queue management, next_hop not needed)
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(to = %to)))]
    pub(super) fn find_peers(&self, to: &hardy_bpv7::eid::Eid) -> Option<HashSet<u32>> {
        let inner = self.inner.read();

        // TODO: this is should be for *all* tables
        let table = &inner.routes;

        if let Some(InternalFindResult::Forward(peers)) =
            find_recurse(table, to, false, &mut HashSet::new(), &self.node_ids.ipn)
        {
            Some(peers.into_iter().map(|(peer, _)| peer).collect())
        } else {
            None
        }
    }

    /// Find all peers reachable via a given EID (for queue management, next_hop not needed)
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(to = %to)))]
    pub fn find_service(
        &self,
        to: &hardy_bpv7::eid::Eid,
    ) -> Option<Arc<services::registry::Service>> {
        let inner = self.inner.read();

        // TODO: this is should be for *all* tables
        let table = &inner.routes;

        for entries in table.values() {
            for (pattern, actions) in entries {
                if pattern_match(pattern, to, &self.node_ids.ipn) {
                    for entry in actions {
                        if let Action::Local(service) = &entry.action {
                            return Some(service.clone());
                        }
                    }
                }
            }
        }
        None
    }
}

fn map_result(
    result: InternalFindResult,
    ecmp_hash_state: &foldhash::quality::RandomState,
    bundle: &hardy_bpv7::bundle::Bundle,
    metadata: &mut bundle::BundleMetadata,
) -> Option<FindResult> {
    match result {
        InternalFindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
        InternalFindResult::Deliver(service) => Some(FindResult::Deliver(service)),
        InternalFindResult::Drop(reason) => Some(FindResult::Drop(reason)),
        InternalFindResult::Forward(peers) => {
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

#[cfg_attr(feature = "instrument", instrument(skip(table, to, trail, local_node),fields(to = %to)))]
fn find_recurse<'a>(
    table: &'a RouteTable,
    to: &'a Eid,
    reflect: bool,
    trail: &mut HashSet<&'a Eid>,
    local_node: &Option<IpnNodeId>,
) -> Option<InternalFindResult<'a>> {
    debug!("Looking for route for {to}");

    let mut peers: Vec<(u32, &'a Eid)> = Vec::new();
    for entries in table.values() {
        for (pattern, actions) in entries {
            if pattern_match(pattern, to, local_node) {
                for entry in actions {
                    match &entry.action {
                        Action::Drop(reason) => {
                            // Drop trumps everything else
                            debug!("Drop {reason:?}");
                            return Some(InternalFindResult::Drop(*reason));
                        }
                        Action::Reflect => {
                            if reflect {
                                debug!("Reflect");
                                return Some(InternalFindResult::Reflect);
                            }
                        }
                        Action::Via(via) => {
                            // Recursive lookup
                            if !trail.insert(to) {
                                warn!("Recursive route {to} found!");
                                return Some(InternalFindResult::Drop(Some(
                                    ReasonCode::NoKnownRouteToDestinationFromHere,
                                )));
                            }

                            let sub_result = find_recurse(table, via, reflect, trail, local_node);

                            trail.remove(to);

                            if let Some(sub_result) = sub_result {
                                let InternalFindResult::Forward(sub_peers) = sub_result else {
                                    // If we find a non-forward, then get out
                                    return Some(sub_result);
                                };

                                // The 'via' Eid is the next-hop for all peers found through this path
                                // Append peers to the running vec, maintaining sort order
                                for (sub_peer, _) in sub_peers {
                                    sorted_insert(&mut peers, sub_peer, via);
                                }
                            } else {
                                // TODO: Kick off a resolver lookup for `via`
                            }
                        }
                        Action::AdminEndpoint => {
                            debug!("Deliver to Admin Endpoint");
                            return Some(InternalFindResult::AdminEndpoint);
                        }
                        Action::Local(service) => {
                            debug!("Deliver to Service {}", service.service_id);
                            return Some(InternalFindResult::Deliver(service.clone()));
                        }
                        Action::Forward(peer) => {
                            // The 'to' Eid is the next-hop for all peers found here
                            sorted_insert(&mut peers, *peer, to);
                        }
                    }
                }
            }
        }

        if !peers.is_empty() {
            return Some(InternalFindResult::Forward(peers));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rib::route::tests::{add_route, make_rib};

    // Add a local forward entry directly (sync, no store interaction).
    fn add_local_forward(rib: &Rib, node_id: hardy_bpv7::eid::NodeId, peer: u32) {
        let pattern: EidPattern = node_id.into();
        add_route(
            rib,
            &pattern.to_string(),
            "forward",
            Action::Forward(peer),
            0,
        )
    }

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
        let mut bundle = make_bundle("ipn:0.2.1");
        let result = rib.find(&mut bundle);
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
            Action::Via("ipn:0.10.0".parse().unwrap()),
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

        // No matching route — unknown destination returns None (wait for route)
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
            Action::Via("ipn:0.3.0".parse().unwrap()),
            10,
        );
        add_route(
            &rib,
            "ipn:0.3.*",
            "loop",
            Action::Via("ipn:0.2.0".parse().unwrap()),
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
        add_route(&rib, "ipn:0.5.*", "reflect", route::Action::Reflect, 10);

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
        add_route(&rib, "ipn:0.5.*", "r", route::Action::Reflect, 10);
        add_route(&rib, "ipn:0.4.*", "r", route::Action::Reflect, 10);

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
            Action::Via("ipn:0.10.0".parse().unwrap()),
            10,
        );
        add_route(
            &rib,
            "ipn:0.50.*",
            "ecmp_b",
            Action::Via("ipn:0.11.0".parse().unwrap()),
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

    #[test]
    fn test_admin_endpoint_lookup() {
        let rib = make_rib();

        // Rib::new() adds admin endpoint routes at priority 0.
        // The IPN admin EID (ipn:0.1.0) should resolve to AdminEndpoint.
        let mut bundle = make_bundle("ipn:0.1.0");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::AdminEndpoint)),
            "Admin EID should resolve to AdminEndpoint, got {result:?}"
        );
    }

    #[test]
    fn test_unregistered_local_waits() {
        let rib = make_rib();

        // A bundle for a local service number with no registered service
        // should return None (wait for route) — not Drop.
        // This is the correct DTN behaviour: default to wait.
        let mut bundle = make_bundle("ipn:0.1.99");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Unregistered local service should wait (no route), got {result:?}"
        );
    }

    #[test]
    fn test_localnode_service_matches_concrete_eid() {
        // A service registered with a concrete local EID should be stored
        // under a LocalNode pattern (via to_local_eid in add_service).
        // The find() lookup with a concrete EID should match via pattern_match.
        let rib = make_rib();

        // Manually add a LocalNode service route (simulating add_service)
        add_route(
            &rib,
            "ipn:!.42",
            "services",
            Action::Local(Arc::new(services::registry::Service {
                service: services::registry::ServiceImpl::LowLevel(Arc::new(
                    crate::services::tests::NullService,
                )),
                service_id: hardy_bpv7::eid::Service::Ipn(42),
            })),
            1,
        );

        // Bundle with concrete local EID should match the LocalNode pattern
        let mut bundle = make_bundle("ipn:0.1.42");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::Deliver(_))),
            "Concrete local EID should match LocalNode service route, got {result:?}"
        );
    }

    #[test]
    fn test_localnode_pattern_ignores_remote_eid() {
        // A LocalNode pattern should NOT match a remote node's EID,
        // even if the service number matches.
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:!.42",
            "services",
            Action::Local(Arc::new(services::registry::Service {
                service: services::registry::ServiceImpl::LowLevel(Arc::new(
                    crate::services::tests::NullService,
                )),
                service_id: hardy_bpv7::eid::Service::Ipn(42),
            })),
            1,
        );

        // Bundle for a different node should NOT match
        let mut bundle = make_bundle("ipn:0.2.42");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Remote EID should not match LocalNode pattern, got {result:?}"
        );
    }

    #[test]
    fn test_admin_endpoint_localnode_matches_concrete() {
        // The admin endpoint is registered as ipn:!.0.
        // A bundle for ipn:0.1.0 (concrete admin EID) should match
        // via pattern_match's LocalNode fallback.
        let rib = make_rib();

        let mut bundle = make_bundle("ipn:0.1.0");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::AdminEndpoint)),
            "Concrete admin EID should match LocalNode(0) pattern, got {result:?}"
        );
    }

    #[test]
    fn test_explicit_drop_overrides_wait() {
        let rib = make_rib();

        // Operator configures an explicit Drop rule for a service range.
        // This overrides the default wait behaviour for unregistered services.
        add_route(
            &rib,
            "ipn:0.1.*",
            "policy",
            Action::Drop(Some(ReasonCode::DestinationEndpointIDUnavailable)),
            10,
        );

        let mut bundle = make_bundle("ipn:0.1.99");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(
                result,
                Some(FindResult::Drop(Some(
                    ReasonCode::DestinationEndpointIDUnavailable
                )))
            ),
            "Explicit drop rule should override default wait, got {result:?}"
        );
    }
}
