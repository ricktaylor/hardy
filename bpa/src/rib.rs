use super::*;
use core::cmp::Reverse;
use hardy_bpv7::{eid::Eid, status_report::ReasonCode};
use hardy_eid_pattern::{EidPattern, EidPatternMap, EidPatternSet};
use rand::prelude::*;
use std::collections::{BinaryHeap, HashMap, HashSet};
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalAction {
    AdminEndpoint,                         // Deliver to the admin endpoint
    Local(Arc<service_registry::Service>), // Deliver to local service
    Forward(cla::ClaAddress, Option<Arc<cla_registry::Cla>>), // Forward to CLA
}

pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<service_registry::Service>), // Deliver to local service
    Forward(
        Vec<(Arc<cla_registry::Cla>, cla::ClaAddress)>, // Available endpoints for forwarding
        Option<time::OffsetDateTime>,                   // Timestamp of next forwarding opportunity
    ),
}

pub enum WaitResult {
    Cancelled,
    Timeout,
    RouteChange,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RouteEntry {
    priority: u32,
    action: routes::Action,
    source: String,
}

#[derive(Debug)]
struct RibInner {
    locals: HashMap<Eid, BinaryHeap<Reverse<LocalAction>>>,
    routes: EidPatternMap<Reverse<RouteEntry>>,
    finals: EidPatternSet,
    address_types: HashMap<cla::ClaAddressType, Arc<cla_registry::Cla>>,
}

#[derive(Debug)]
pub struct Rib {
    inner: RwLock<RibInner>,
    cancellable_waits: Mutex<HashMap<Eid, tokio_util::sync::CancellationToken>>,
}

impl Rib {
    pub fn new(config: &config::Config) -> Arc<Self> {
        let mut locals = HashMap::new();
        let mut finals = EidPatternSet::new();

        // Add localnode admin endpoint
        locals.insert(
            Eid::LocalNode { service_number: 0 },
            vec![Reverse(LocalAction::AdminEndpoint)].into(),
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
                vec![Reverse(LocalAction::AdminEndpoint)].into(),
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
                    demux: [].into(),
                },
                vec![Reverse(LocalAction::AdminEndpoint)].into(),
            );

            finals.insert(format!("dtn://{node_name}/**").parse().unwrap());
        }

        Arc::new(Self {
            inner: RwLock::new(RibInner {
                locals,
                finals,
                routes: EidPatternMap::new(),
                address_types: HashMap::new(),
            }),
            cancellable_waits: Mutex::default(),
        })
    }

    pub async fn add(
        &self,
        pattern: EidPattern,
        source: String,
        action: routes::Action,
        priority: u32,
    ) {
        info!("Adding route {pattern} => {action:?}, priority {priority}, source '{source}'");

        {
            self.inner.write().await.routes.insert(
                pattern.clone(),
                Reverse(RouteEntry {
                    source,
                    action,
                    priority,
                }),
            )
        }

        // Wake all waiters
        self.wake(pattern.into()).await
    }

    async fn add_local(&self, eid: Eid, action: LocalAction) {
        info!("Adding local route {eid} => {action:?}");

        match self.inner.write().await.locals.entry(eid.clone()) {
            std::collections::hash_map::Entry::Occupied(mut occupied_entry) => {
                occupied_entry.get_mut().push(Reverse(action));
            }
            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(vec![Reverse(action)].into());
            }
        }

        // Wake all waiters
        if let Some(token) = self.cancellable_waits.lock().await.remove(&eid) {
            token.cancel();
        }
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

    pub async fn remove(
        &self,
        pattern: &EidPattern,
        source: &str,
        action: &routes::Action,
        priority: u32,
    ) -> bool {
        let v = {
            self.inner
                .write()
                .await
                .routes
                .remove_if::<Vec<_>, _>(pattern, |e| {
                    e.0.source == source && e.0.priority == priority && &e.0.action == action
                })
        };

        for v in &v {
            info!(
                "Removed route {pattern} => {:?}, priority {priority}, source '{source}'",
                v.0.action
            )
        }

        if v.is_empty() {
            false
        } else {
            if let routes::Action::Store(_) = action {
                // Wake all waiters, we have changed a wait time
                self.wake(pattern.clone().into()).await
            }
            true
        }
    }

    async fn remove_local(&self, eid: &Eid, mut f: impl FnMut(&mut LocalAction) -> bool) -> bool {
        let v = self
            .inner
            .write()
            .await
            .locals
            .get_mut(eid)
            .map(|h| {
                let mut v = std::mem::take(h).into_vec();
                let r = v.extract_if(.., |a| f(&mut a.0)).collect::<Vec<_>>();
                *h = v.into();
                r
            })
            .unwrap_or_default();

        for v in &v {
            info!("Removed route {eid} => {:?}", v.0)
        }
        !v.is_empty()
    }

    pub async fn remove_forward(
        &self,
        eid: &Eid,
        cla_addr: &cla::ClaAddress,
        cla: Option<&Arc<cla_registry::Cla>>,
    ) -> bool {
        self.remove_local(eid, |action| match action {
            LocalAction::Forward(addr, c) => addr == cla_addr && c.as_ref() == cla,
            _ => false,
        })
        .await
    }

    pub async fn remove_service(&self, eid: &Eid, service: &service_registry::Service) -> bool {
        self.remove_local(eid, |action| {
            if let LocalAction::Local(svc) = action {
                svc.as_ref() == service
            } else {
                false
            }
        })
        .await
    }

    pub async fn add_address_type(
        &self,
        address_type: cla::ClaAddressType,
        cla: Arc<cla_registry::Cla>,
    ) {
        self.inner
            .write()
            .await
            .address_types
            .insert(address_type, cla);
    }

    pub async fn remove_address_type(&self, address_type: &cla::ClaAddressType) {
        self.inner.write().await.address_types.remove(address_type);
    }

    pub async fn find(&self, to: &Eid) -> Result<Option<FindResult>, Option<ReasonCode>> {
        let mut result = {
            let inner = self.inner.read().await;
            find_recurse(&inner, to, &mut HashSet::new())?
        };

        if let Some(FindResult::Forward(clas, _)) = &mut result {
            // For ECMP, we need a random order
            clas.shuffle(&mut rand::rng());
        }

        Ok(result)
    }

    pub async fn wait_for_route(
        &self,
        to: &Eid,
        duration: std::time::Duration,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) -> WaitResult {
        let token = self
            .cancellable_waits
            .lock()
            .await
            .entry(to.clone())
            .or_insert(tokio_util::sync::CancellationToken::new())
            .clone();

        // Wait a bit
        let timer = tokio::time::sleep(duration);
        tokio::pin!(timer);

        tokio::select! {
            () = &mut timer => WaitResult::Timeout,
            _ = cancel_token.cancelled() => WaitResult::Cancelled,
            _ = token.cancelled() => {
                // Remove the token from the map
                self.cancellable_waits
                    .lock()
                    .await.remove(to);
                WaitResult::RouteChange
            }
        }
    }

    async fn wake(&self, pattern: EidPatternSet) {
        for token in self
            .cancellable_waits
            .lock()
            .await
            .extract_if(|eid, _| pattern.contains(eid))
            .map(|(_, token)| token)
        {
            token.cancel();
        }
    }
}

fn find_local<'a>(inner: &'a RibInner, to: &'a Eid) -> Option<FindResult> {
    let mut clas: Option<Vec<(Arc<cla_registry::Cla>, cla::ClaAddress)>> = None;
    for action in inner.locals.get(to).into_iter().flatten() {
        match &action.0 {
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
    clas.map(|clas| FindResult::Forward(clas, None))
}

#[instrument(skip(inner, trail))]
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
    for entry in inner.routes.find::<std::collections::BinaryHeap<_>>(to) {
        // Ensure we only look at lowest priority values
        if let Some(priority) = priority {
            if entry.0.priority > priority {
                break;
            }
        } else {
            priority = Some(entry.0.priority);
        }

        match &entry.0.action {
            routes::Action::Via(via) => {
                // Recusive lookup
                if let Some(sub_result) = find_recurse(inner, via, trail)? {
                    let FindResult::Forward(sub_clas, sub_until) = sub_result else {
                        // If we find a non-forward, then break
                        result = Some(sub_result);
                        break;
                    };

                    if let Some(FindResult::Forward(clas, until)) = &mut result {
                        clas.extend(sub_clas);

                        if let Some(sub_until) = sub_until {
                            if let Some(until) = until {
                                if sub_until < *until {
                                    *until = sub_until
                                }
                            } else {
                                *until = Some(sub_until);
                            }
                        }
                    } else {
                        result = Some(FindResult::Forward(sub_clas, sub_until));
                    }
                }
            }
            routes::Action::Store(sub_until) => {
                if *sub_until >= time::OffsetDateTime::now_utc() {
                    if let Some(FindResult::Forward(_, until)) = &mut result {
                        if let Some(until) = until {
                            if sub_until < until {
                                *until = *sub_until
                            }
                        } else {
                            *until = Some(*sub_until);
                        }
                    } else {
                        result = Some(FindResult::Forward(Vec::new(), Some(*sub_until)));
                    }
                }

                // The sort ensures that this is the shortest wait and have processed everything else relevant already
                break;
            }
            routes::Action::Drop(reason) => {
                // Drop trumps everything else
                return Err(*reason);
            }
        }

        trail.remove(to);
    }

    if result.is_none() && inner.finals.contains(to) {
        return Err(Some(ReasonCode::DestinationEndpointIDUnavailable));
    }
    Ok(result)
}
