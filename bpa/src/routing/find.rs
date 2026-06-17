use core::hash::BuildHasher;

use foldhash::quality::RandomState;
use hardy_bpv7::{bundle::Bundle as Bpv7Bundle, eid::Eid, status_report::ReasonCode};
use tracing::trace;

#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{FindResult, Rib, RouteTable, route::Action};
use crate::{Arc, HashSet, bundle, services};

impl Rib {
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    pub fn find(&self, bundle: &mut bundle::Bundle) -> Option<FindResult> {
        let inner = self.inner.read();

        let result =
            inner
                .table
                .find_recurse(&bundle.bundle.destination, true, &mut HashSet::new())?;
        if !matches!(result, table::FindResult::Reflect) {
            return map_result(
                result,
                &self.ecmp_hash_state,
                &bundle.bundle,
                &mut bundle.metadata,
            );
        }

        let previous = bundle
            .previous_node()
            .unwrap_or_else(|| bundle.bundle.id.source.clone());

        map_result(
            inner
                .table
                .find_recurse(&previous, false, &mut HashSet::new())?,
            &self.ecmp_hash_state,
            &bundle.bundle,
            &mut bundle.metadata,
        )
    }

    /// Find all peers reachable via a given EID (for queue management, next_hop not needed)
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(to = %to)))]
    pub(super) fn find_peers(&self, to: &Eid) -> Option<HashSet<u32>> {
        let inner = self.inner.read();
        inner.table.find_peers(to)
    }

    /// Find a registered local service matching the given EID.
    ///
    /// Used for status report notifications (`admin.rs`) where we need to
    /// find the service to notify, regardless of routing policy. This
    /// intentionally bypasses priority ordering and Drop rules — a Drop
    /// rule prevents *routing* bundles to a service, but should not prevent
    /// the BPA from notifying a registered service about its own bundles.
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(to = %to)))]
    pub fn find_service(&self, to: &Eid) -> Option<Arc<services::registry::Service>> {
        let inner = self.inner.read();
        inner.table.find_service(to)
    }
}

fn map_result(
    result: InternalFindResult,
    ecmp_hash_state: &RandomState,
    bundle: &Bpv7Bundle,
    metadata: &mut bundle::BundleMetadata,
) -> Option<FindResult> {
    match result {
        table::FindResult::AdminEndpoint => Some(FindResult::AdminEndpoint),
        table::FindResult::Deliver(service) => Some(FindResult::Deliver(service)),
        table::FindResult::Drop(reason) => Some(FindResult::Drop(reason)),
        table::FindResult::Forward(peers) if peers.is_empty() => {
            debug_assert!(false, "Empty Forward result from find_recurse");
            None
        }
        table::FindResult::Forward(mut peers) => {
            if tracing::enabled!(tracing::Level::TRACE) {
                trace!(
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
            }

            let idx = if peers.len() > 1 {
                (ecmp_hash_state.hash_one((
                    &bundle.id.source,
                    &bundle.destination,
                    &metadata.writable.flow_label,
                )) % (peers.len() as u64)) as usize
            } else {
                0
            };
            let (peer, next_hop) = peers.swap_remove(idx);

            metadata.read_only.next_hop = Some(next_hop.clone());

            Some(FindResult::Forward(peer))
        }
        table::FindResult::Reflect => None,
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use hardy_bpv7::{
        bundle::{Bundle as Bpv7Bundle, Id as BundleId},
        creation_timestamp::CreationTimestamp,
        eid::{IpnNodeId, NodeId, Service as EidService},
    };
    use hardy_eid_patterns::EidPattern;

    use super::*;
    use crate::{
        rib::route::{
            self,
            tests::{add_route, make_rib},
        },
        services::tests::NullService,
    };

    // Add a local forward entry directly (sync, no store interaction).
    fn add_local_forward(rib: &Rib, node_id: NodeId, peer: u32) {
        let pattern: EidPattern = node_id.into();
        add_route(
            rib,
            &pattern.to_string(),
            "forward",
            Action::Internal(InternalAction::Forward(peer)),
            0,
        )
    }

    fn make_bundle(destination: &str) -> bundle::Bundle {
        bundle::Bundle {
            bundle: Bpv7Bundle {
                id: BundleId {
                    source: "ipn:0.99.1".parse().unwrap(),
                    timestamp: CreationTimestamp::now(),
                    fragment_info: None,
                },
                flags: Default::default(),
                crc_type: Default::default(),
                destination: destination.parse().unwrap(),
                report_to: Default::default(),
                lifetime: Duration::from_secs(3600),
                previous_node: None,
                age: None,
                hop_count: None,
                blocks: Default::default(),
            },
            metadata: Default::default(),
        }
    }

    fn ipn_node(n: u32) -> NodeId {
        NodeId::Ipn(IpnNodeId {
            allocator_id: 0,
            node_number: n,
        })
    }

    #[test]
    fn test_exact_match() {
        let rib = make_rib();
        add_local_forward(&rib, ipn_node(2), 42);

        let mut bundle = make_bundle("ipn:0.2.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(42))));
    }

    #[test]
    fn test_default_route() {
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.50.*",
            "default",
            Action::Route(RouteAction::Via("ipn:0.10.0".parse().unwrap())),
            1000,
        );

        add_local_forward(&rib, ipn_node(10), 99);

        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(99))));
    }

    #[test]
    fn test_no_route() {
        let rib = make_rib();

        let mut bundle = make_bundle("ipn:0.50.1");
        let result = rib.find(&mut bundle);
        assert!(result.is_none());
    }

    #[test]
    fn test_recursion_loop() {
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.2.*",
            "loop",
            Action::Route(RouteAction::Via("ipn:0.3.0".parse().unwrap())),
            10,
        );
        add_route(
            &rib,
            "ipn:0.3.*",
            "loop",
            Action::Route(RouteAction::Via("ipn:0.2.0".parse().unwrap())),
            10,
        );

        let mut bundle = make_bundle("ipn:0.2.1");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Recursive route should return None (wait), not Drop"
        );
    }

    #[test]
    fn test_reflection() {
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.5.*",
            "reflect",
            Action::Route(RouteAction::Reflect),
            10,
        );

        add_local_forward(&rib, ipn_node(4), 77);

        let mut bundle = make_bundle("ipn:0.5.1");
        bundle.bundle.previous_node = Some("ipn:0.4.0".parse().unwrap());

        let result = rib.find(&mut bundle);
        assert!(matches!(result, Some(FindResult::Forward(77))));
    }

    #[test]
    fn test_reflection_no_double() {
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.5.*",
            "r",
            Action::Route(RouteAction::Reflect),
            10,
        );
        add_route(
            &rib,
            "ipn:0.4.*",
            "r",
            Action::Route(RouteAction::Reflect),
            10,
        );

        let mut bundle = make_bundle("ipn:0.5.1");
        bundle.bundle.previous_node = Some("ipn:0.4.0".parse().unwrap());

        let result = rib.find(&mut bundle);
        assert!(result.is_none());
    }

    #[test]
    fn test_ecmp_hashing() {
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.50.*",
            "ecmp_a",
            Action::Route(RouteAction::Via("ipn:0.10.0".parse().unwrap())),
            10,
        );
        add_route(
            &rib,
            "ipn:0.50.*",
            "ecmp_b",
            Action::Route(RouteAction::Via("ipn:0.11.0".parse().unwrap())),
            10,
        );

        add_local_forward(&rib, ipn_node(10), 10);
        add_local_forward(&rib, ipn_node(11), 11);

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

        let mut bundle = make_bundle("ipn:0.1.99");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Unregistered local service should wait (no route), got {result:?}"
        );
    }

    #[test]
    fn test_concrete_service_matches() {
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.1.42",
            "services",
            Action::Internal(InternalAction::Local(Arc::new(
                services::registry::Service {
                    service: services::registry::ServiceImpl::LowLevel(Arc::new(NullService)),
                    service_id: EidService::Ipn(42),
                },
            ))),
            1,
        );

        let mut bundle = make_bundle("ipn:0.1.42");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::Deliver(_))),
            "Concrete local EID should match concrete service route, got {result:?}"
        );
    }

    #[test]
    fn test_concrete_service_ignores_remote_eid() {
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.1.42",
            "services",
            Action::Internal(InternalAction::Local(Arc::new(
                services::registry::Service {
                    service: services::registry::ServiceImpl::LowLevel(Arc::new(NullService)),
                    service_id: EidService::Ipn(42),
                },
            ))),
            1,
        );

        let mut bundle = make_bundle("ipn:0.2.42");
        let result = rib.find(&mut bundle);
        assert!(
            result.is_none(),
            "Remote EID should not match local service route, got {result:?}"
        );
    }

    #[test]
    fn test_admin_endpoint_matches_concrete() {
        let rib = make_rib();

        let mut bundle = make_bundle("ipn:0.1.0");
        let result = rib.find(&mut bundle);
        assert!(
            matches!(result, Some(FindResult::AdminEndpoint)),
            "Concrete admin EID should match admin endpoint route, got {result:?}"
        );
    }

    #[test]
    fn test_explicit_drop_overrides_wait() {
        let rib = make_rib();

        add_route(
            &rib,
            "ipn:0.1.*",
            "policy",
            Action::Route(RouteAction::Drop(Some(
                ReasonCode::DestinationEndpointIDUnavailable,
            ))),
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
