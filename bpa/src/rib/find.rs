use super::*;
use core::hash::BuildHasher;

#[derive(Debug)]
enum InternalFindResult<'a> {
    AdminEndpoint,
    Deliver(Option<Arc<services::registry::Service>>), // Deliver to local service
    Forward(Vec<(Arc<cla::adapter::Adapter>, &'a Eid)>), // CLA entry -> next_hop pairs
    Drop(Option<ReasonCode>),                          // Drop with reason code
    Reflect,                                           // Reflect
}

/// Insert into a sorted vec, maintaining sort order by CLA name. Skips duplicates.
fn sorted_insert<'a>(
    peers: &mut Vec<(Arc<cla::adapter::Adapter>, &'a Eid)>,
    entry: Arc<cla::adapter::Adapter>,
    next_hop: &'a Eid,
) {
    if !peers.iter().any(|(e, _)| e == &entry) {
        peers.push((entry, next_hop));
        peers.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    }
}

impl Rib {
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
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

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(to = %to)))]
    pub fn find_local(&self, to: &Eid) -> Option<FindResult> {
        let inner = self.inner.read();

        let result = find_local_inner(&inner, to)?;
        match result {
            InternalFindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
            InternalFindResult::Deliver(service) => Some(FindResult::Deliver(service)),
            InternalFindResult::Forward(peers) => Some(FindResult::Forward(
                peers
                    .first()
                    .trace_expect("Forward with empty peers!")
                    .0
                    .clone(),
            )),
            InternalFindResult::Drop(reason) => Some(FindResult::Drop(reason)),
            InternalFindResult::Reflect => {
                unreachable!("Reflect filtered by find_local before calling map_result")
            }
        }
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
            let selected = if peers.len() > 1 {
                let idx = (ecmp_hash_state.hash_one((
                    &bundle.id.source,
                    &bundle.destination,
                    &metadata.writable.flow_label,
                )) % (peers.len() as u64)) as usize;
                peers
                    .get(idx)
                    .trace_expect("ECMP hash has picked an invalid entry")
            } else {
                peers.first().trace_expect("Empty CLA result from find?!?")
            };

            metadata.read_only.next_hop = Some(selected.1.clone());

            Some(FindResult::Forward(selected.0.clone()))
        }
        InternalFindResult::Reflect => {
            unreachable!("Reflect filtered by find() before calling map_result")
        }
    }
}

#[cfg_attr(feature = "instrument", instrument(skip_all,fields(to = %to)))]
fn find_local_inner<'a>(inner: &'a RibInner, to: &'a Eid) -> Option<InternalFindResult<'a>> {
    let mut peers: Option<Vec<(Arc<cla::adapter::Adapter>, &'a Eid)>> = None;

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
                    local::Action::Forward(adapter) => {
                        if let Some(peers) = &mut peers {
                            sorted_insert(peers, adapter.clone(), to);
                        } else {
                            peers = Some(vec![(adapter.clone(), to)]);
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

#[cfg_attr(feature = "instrument", instrument(skip(inner, table, to, trail),fields(to = %to)))]
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
                                    for (entry, _) in sub_peers {
                                        sorted_insert(peers, entry, via);
                                    }
                                } else {
                                    let mut peers: Vec<(Arc<cla::adapter::Adapter>, &'a Eid)> =
                                        sub_peers
                                            .into_iter()
                                            .map(|(entry, _)| (entry, via))
                                            .collect();
                                    peers.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
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
    use super::super::tests::make_adapter;
    use super::*;
    use rib::tests::{add_local_forward, add_route, make_rib};

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
        let cla = make_adapter("test-cla");

        add_local_forward(&rib, ipn_node(2), cla.clone());

        let result = rib.find_local(&"ipn:0.2.1".parse().unwrap());
        assert!(
            matches!(result, Some(FindResult::Forward(ref e)) if e.name.as_ref() == "test-cla")
        );
    }

    #[test]
    fn test_default_route() {
        let rib = make_rib();
        let cla = make_adapter("gw-cla");

        add_route(
            &rib,
            "*:**",
            "default",
            routes::Action::Via("ipn:0.10.0".parse().unwrap()),
            1000,
        );

        add_local_forward(&rib, ipn_node(10), cla);

        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(ref e)) if e.name.as_ref() == "gw-cla"));
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

        let cla = make_adapter("reflect-cla");
        add_local_forward(&rib, ipn_node(4), cla);

        let mut bundle = make_bundle("ipn:0.5.1");
        bundle.bundle.previous_node = Some("ipn:0.4.0".parse().unwrap());

        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::Forward(ref e)) if e.name.as_ref() == "reflect-cla")
        );
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

        let cla_a = make_adapter("ecmp-a");
        let cla_b = make_adapter("ecmp-b");
        add_local_forward(&rib, ipn_node(10), cla_a);
        add_local_forward(&rib, ipn_node(11), cla_b);

        // Same bundle should always hash to the same CLA (deterministic)
        let mut bundle = make_bundle("ipn:0.50.1");
        let result1 = rib.find(&mut bundle);
        let name1 = match &result1 {
            Some(FindResult::Forward(e)) => e.name.clone(),
            other => panic!("Expected Forward, got {other:?}"),
        };

        let mut bundle2 = make_bundle("ipn:0.50.1");
        bundle2.bundle.id = bundle.bundle.id.clone();
        let result2 = rib.find(&mut bundle2);
        let name2 = match &result2 {
            Some(FindResult::Forward(e)) => e.name.clone(),
            other => panic!("Expected Forward, got {other:?}"),
        };

        assert_eq!(name1, name2, "ECMP selection must be deterministic");
        assert!(
            name1.as_ref() == "ecmp-a" || name1.as_ref() == "ecmp-b",
            "CLA must be one of the ECMP targets, got {name1}"
        );
    }
}
