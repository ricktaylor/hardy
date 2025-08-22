use super::*;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use hardy_eid_pattern::{EidPattern, EidPatternMap, EidPatternSet};
use rand::prelude::*;
use std::{
    collections::{BinaryHeap, HashMap, HashSet},
    sync::RwLock,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LocalAction {
    AdminEndpoint,                         // Deliver to the admin endpoint
    Local(Arc<service_registry::Service>), // Deliver to local service
    Forward(cla::ClaAddress, Option<Arc<cla_registry::Cla>>), // Forward to CLA
}

impl PartialOrd for LocalAction {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LocalAction {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // The order is critical, hence done long-hand
        match (self, other) {
            (LocalAction::AdminEndpoint, LocalAction::AdminEndpoint) => std::cmp::Ordering::Equal,
            (LocalAction::AdminEndpoint, LocalAction::Local(_))
            | (LocalAction::AdminEndpoint, LocalAction::Forward(..)) => std::cmp::Ordering::Less,
            (LocalAction::Local(_), LocalAction::AdminEndpoint) => std::cmp::Ordering::Greater,
            (LocalAction::Local(lhs), LocalAction::Local(rhs)) => lhs.cmp(rhs),
            (LocalAction::Local(_), LocalAction::Forward(..)) => std::cmp::Ordering::Less,
            (LocalAction::Forward(..), LocalAction::AdminEndpoint)
            | (LocalAction::Forward(..), LocalAction::Local(_)) => std::cmp::Ordering::Greater,
            (LocalAction::Forward(lhs_addr, lhs), LocalAction::Forward(rhs_addr, rhs)) => {
                lhs_addr.cmp(rhs_addr).then_with(|| lhs.cmp(rhs))
            }
        }
        .reverse()
        // BinaryHeap is a max-heap!
    }
}

impl std::fmt::Display for LocalAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LocalAction::AdminEndpoint => write!(f, "administrative endpoint"),
            LocalAction::Local(service) => write!(f, "local service {}", &service.service_id),
            LocalAction::Forward(cla_address, cla) => {
                if let Some(cla) = cla {
                    write!(f, "forward via {}", cla.name)
                } else {
                    write!(f, "forward to {cla_address}")
                }
            }
        }
    }
}

pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<service_registry::Service>), // Deliver to local service
    Forward(
        Vec<(Arc<cla_registry::Cla>, cla::ClaAddress)>, // Available endpoints for forwarding
        bool,                                           // Should we reflect if forwarding fails
    ),
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct RouteEntry {
    priority: u32,
    action: routes::Action,
    source: String,
}

impl PartialOrd for RouteEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RouteEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // The order is critical, hence done long-hand
        self.priority
            .cmp(&other.priority)
            .then_with(|| match (&self.action, &other.action) {
                (routes::Action::Drop(lhs), routes::Action::Drop(rhs)) => lhs.cmp(rhs),
                (routes::Action::Drop(_), routes::Action::Reflect)
                | (routes::Action::Drop(_), routes::Action::Via(_)) => std::cmp::Ordering::Less,
                (routes::Action::Reflect, routes::Action::Drop(_)) => std::cmp::Ordering::Greater,
                (routes::Action::Reflect, routes::Action::Reflect) => std::cmp::Ordering::Equal,
                (routes::Action::Reflect, routes::Action::Via(_)) => std::cmp::Ordering::Less,
                (routes::Action::Via(_), routes::Action::Drop(_))
                | (routes::Action::Via(_), routes::Action::Reflect) => std::cmp::Ordering::Greater,
                (routes::Action::Via(lhs), routes::Action::Via(rhs)) => lhs.cmp(rhs),
            })
            .reverse()
        // BinaryHeap is a max-heap!
    }
}

struct RibInner {
    locals: HashMap<Eid, BinaryHeap<LocalAction>>,
    routes: EidPatternMap<RouteEntry>,
    finals: EidPatternSet,
    address_types: HashMap<cla::ClaAddressType, Arc<cla_registry::Cla>>,
}

pub struct Rib {
    inner: RwLock<RibInner>,
    sentinel: Arc<sentinel::Sentinel>,
}

impl Rib {
    pub fn new(config: &config::Config, sentinel: Arc<sentinel::Sentinel>) -> Self {
        let mut locals = HashMap::new();
        let mut finals = EidPatternSet::new();

        // Add localnode admin endpoint
        locals.insert(
            Eid::LocalNode { service_number: 0 },
            vec![LocalAction::AdminEndpoint].into(),
        );

        // Drop LocalNode services
        finals.insert("ipn:!.*".parse().unwrap());

        if let Some((allocator_id, node_number)) = config.node_ids.ipn {
            // Add the Admin Endpoint EID itself
            locals.insert(
                Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number: 0,
                },
                vec![LocalAction::AdminEndpoint].into(),
            );

            finals.insert(
                format!("ipn:{allocator_id}.{node_number}.*")
                    .parse()
                    .unwrap(),
            );
        }

        if let Some(node_name) = &config.node_ids.dtn {
            // Add the Admin Endpoint EID itself
            locals.insert(
                Eid::Dtn {
                    node_name: node_name.clone(),
                    demux: "".into(),
                },
                vec![LocalAction::AdminEndpoint].into(),
            );

            finals.insert(format!("dtn://{node_name}/**").parse().unwrap());
        }

        Self {
            inner: RwLock::new(RibInner {
                locals,
                finals,
                routes: EidPatternMap::new(),
                address_types: HashMap::new(),
            }),
            sentinel,
        }
    }

    pub async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: routes::Action,
        priority: u32,
    ) {
        info!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");

        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .routes
            .insert(
                pattern.clone(),
                RouteEntry {
                    source,
                    action,
                    priority,
                },
            );

        self.sentinel.new_route(pattern).await
    }

    async fn add_local(&self, eid: Eid, action: LocalAction) {
        info!("Adding local route {eid} => {action}");

        match self
            .inner
            .write()
            .trace_expect("Failed to lock mutex")
            .locals
            .entry(eid.clone())
        {
            std::collections::hash_map::Entry::Occupied(mut occupied_entry) => {
                occupied_entry.get_mut().push(action);
            }
            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(vec![action].into());
            }
        }

        self.sentinel.new_route(eid.into()).await
    }

    pub async fn add_forward(
        &self,
        eid: Eid,
        cla_addr: cla::ClaAddress,
        cla: Option<Arc<cla_registry::Cla>>,
    ) {
        self.add_local(eid, LocalAction::Forward(cla_addr, cla))
            .await
    }

    pub async fn add_service(&self, eid: Eid, service: Arc<service_registry::Service>) {
        self.add_local(eid, LocalAction::Local(service)).await
    }

    pub fn remove(
        &self,
        pattern: &EidPattern,
        source: &str,
        action: &routes::Action,
        priority: u32,
    ) -> bool {
        let v = {
            self.inner
                .write()
                .trace_expect("Failed to lock mutex")
                .routes
                .remove_if::<Vec<_>>(pattern, |e| {
                    e.source == source && e.priority == priority && &e.action == action
                })
        };

        for v in &v {
            info!(
                "Removed route {pattern} => {}, priority {priority}, source '{source}'",
                v.action
            )
        }

        !v.is_empty()
    }

    fn remove_local(&self, eid: &Eid, mut f: impl FnMut(&LocalAction) -> bool) -> bool {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .locals
            .get_mut(eid)
            .map(|h| {
                let mut removed = false;
                h.retain(|a| {
                    if f(a) {
                        info!("Removed route {eid} => {a}");
                        removed = true;
                        false
                    } else {
                        true
                    }
                });
                removed
            })
            .unwrap_or(false)
    }

    pub fn remove_forward(
        &self,
        eid: &Eid,
        cla_addr: &cla::ClaAddress,
        cla: Option<&Arc<cla_registry::Cla>>,
    ) -> bool {
        self.remove_local(eid, |action| match action {
            LocalAction::Forward(addr, c) => addr == cla_addr && c.as_ref() == cla,
            _ => false,
        })
    }

    pub fn remove_service(&self, eid: &Eid, service: &service_registry::Service) -> bool {
        self.remove_local(eid, |action| {
            if let LocalAction::Local(svc) = action {
                svc.as_ref() == service
            } else {
                false
            }
        })
    }

    pub fn add_address_type(&self, address_type: cla::ClaAddressType, cla: Arc<cla_registry::Cla>) {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .address_types
            .insert(address_type, cla);
    }

    pub fn remove_address_type(&self, address_type: &cla::ClaAddressType) {
        self.inner
            .write()
            .trace_expect("Failed to lock mutex")
            .address_types
            .remove(address_type);
    }

    #[instrument(level = "trace", skip(self))]
    pub fn find(&self, to: &Eid) -> Result<Option<FindResult>, Option<ReasonCode>> {
        let mut result = find_recurse(
            &self.inner.read().trace_expect("Failed to lock mutex"),
            to,
            &mut HashSet::new(),
        )?;

        if let Some(FindResult::Forward(clas, _)) = &mut result {
            // For ECMP, we need a random order
            clas.shuffle(&mut rand::rng());
        }

        Ok(result)
    }
}

#[instrument(level = "trace", skip(inner))]
fn find_local<'a>(inner: &'a RibInner, to: &'a Eid) -> Option<FindResult> {
    let mut clas: Option<Vec<(Arc<cla_registry::Cla>, cla::ClaAddress)>> = None;
    for action in inner.locals.get(to).into_iter().flatten() {
        match &action {
            LocalAction::AdminEndpoint => {
                return Some(FindResult::AdminEndpoint);
            }
            LocalAction::Local(service) => {
                return Some(FindResult::Deliver(service.clone()));
            }
            LocalAction::Forward(cla_addr, cla) => {
                let f = if let Some(cla) = cla {
                    Some(cla.clone())
                } else {
                    inner.address_types.get(&cla_addr.address_type()).cloned()
                }
                .map(|cla| (cla, cla_addr.clone()));
                if let Some(f) = f {
                    if let Some(clas) = &mut clas {
                        clas.push(f);
                    } else {
                        clas = Some(vec![f]);
                    }
                }
            }
        }
    }
    clas.map(|clas| FindResult::Forward(clas, false))
}

#[instrument(level = "trace", skip(inner, trail))]
fn find_recurse<'a>(
    inner: &'a RibInner,
    to: &'a Eid,
    trail: &mut HashSet<&'a Eid>,
) -> Result<Option<FindResult>, Option<ReasonCode>> {
    // Recursion check
    if !trail.insert(to) {
        warn!("Recursive route {to} found!");
        return Err(Some(ReasonCode::NoKnownRouteToDestinationFromHere));
    }

    // Always check locals first
    let mut result = find_local(inner, to);
    if result.is_some() {
        return Ok(result);
    }

    // Now check routes (this is where route table switching can occur)

    let mut priority = None;
    for entry in inner
        .routes
        .find(to)
        .collect::<std::collections::BinaryHeap<_>>()
    {
        // Ensure we only look at lowest priority values
        if let Some(priority) = priority {
            if entry.priority > priority {
                break;
            }
        } else {
            priority = Some(entry.priority);
        }

        match &entry.action {
            routes::Action::Drop(reason) => {
                // Drop trumps everything else
                return Err(*reason);
            }
            routes::Action::Reflect => {
                if let Some(FindResult::Forward(_, reflect)) = &mut result {
                    *reflect = true;
                } else {
                    result = Some(FindResult::Forward(Vec::new(), true));
                }
            }
            routes::Action::Via(via) => {
                // Recursive lookup
                if let Some(sub_result) = find_recurse(inner, via, trail)? {
                    match sub_result {
                        FindResult::AdminEndpoint | FindResult::Deliver(_) => {
                            // If we find a non-forward, then break
                            result = Some(sub_result);
                            break;
                        }
                        FindResult::Forward(sub_clas, sub_reflect) => {
                            // Append clas to the running set of clas
                            if let Some(FindResult::Forward(clas, reflect)) = &mut result {
                                clas.extend(sub_clas);
                                *reflect = *reflect || sub_reflect;
                            } else {
                                result = Some(FindResult::Forward(sub_clas, sub_reflect));
                            }
                        }
                    }
                }
            }
        }

        trail.remove(to);
    }

    if result.is_none() && inner.finals.contains(to) {
        return Err(Some(ReasonCode::DestinationEndpointIDUnavailable));
    }
    Ok(result)
}
