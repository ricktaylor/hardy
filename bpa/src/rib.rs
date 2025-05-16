use super::*;
use rand::prelude::*;
use std::collections::{HashMap, HashSet};
use tokio::sync::{Mutex, RwLock};

static LOCAL_SOURCE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| "system".to_string());

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RibAction {
    AdminEndpoint,                              // Deliver to the admin endpoint
    Local(Arc<service_registry::Service>),      // Deliver to local service
    Forward(Arc<cla_registry::Cla>),            // Forward to CLA
    Via(bpv7::Eid),                             // Recursive lookup
    Store(time::OffsetDateTime),                // Wait for later availability
    Drop(Option<bpv7::StatusReportReasonCode>), // Drop the bundle
}

impl From<routes::Action> for RibAction {
    fn from(action: routes::Action) -> Self {
        match action {
            routes::Action::Drop(reason) => Self::Drop(reason),
            routes::Action::Via(eid) => Self::Via(eid),
            routes::Action::Store(until) => Self::Store(until),
        }
    }
}

pub enum FindResult {
    AdminEndpoint,
    Deliver(Arc<service_registry::Service>), // Deliver to local service
    Forward(
        Vec<Arc<cla_registry::Cla>>,  // Available endpoints for forwarding
        Option<time::OffsetDateTime>, // Timestamp of next forwarding opportunity
    ),
}

impl Default for FindResult {
    fn default() -> Self {
        Self::Forward(Vec::new(), None)
    }
}

pub enum WaitResult {
    Cancelled,
    Timeout,
    RouteChange,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Entry {
    pattern: eid_pattern::EidPattern,
    priority: u32,
    action: RibAction,
    source: String,
}

#[derive(Debug)]
pub struct Rib {
    routes: RwLock<eid_pattern::EidPatternMap<Entry>>,
    cancellable_waits: Mutex<HashMap<bpv7::Eid, tokio_util::sync::CancellationToken>>,
}

impl Rib {
    pub fn new(config: &config::Config) -> Arc<Self> {
        let mut routes = eid_pattern::EidPatternMap::new();
        let mut add_pattern = |pattern: hardy_eid_pattern::EidPattern, action: RibAction| {
            routes.insert(
                pattern.clone(),
                Entry {
                    pattern,
                    source: LOCAL_SOURCE.clone(),
                    action,
                    priority: 0,
                },
            );
        };

        // Drop Eid::Null silently to cull spam
        add_pattern(bpv7::Eid::Null.into(), RibAction::Drop(None));

        // Drop LocalNode services
        add_pattern(
            "ipn:!.*".parse().unwrap(),
            RibAction::Drop(Some(
                bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable,
            )),
        );

        // Add localnode admin endpoint
        add_pattern(
            bpv7::Eid::LocalNode { service_number: 0 }.into(),
            RibAction::AdminEndpoint,
        );

        if let Some((allocator_id, node_number)) = config.node_ids.ipn {
            // Add the Admin Endpoint EID itself
            add_pattern(
                bpv7::Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number: 0,
                }
                .into(),
                RibAction::AdminEndpoint,
            );

            add_pattern(
                format!("ipn:{allocator_id}.{node_number}.*")
                    .parse()
                    .unwrap(),
                RibAction::Drop(Some(
                    bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable,
                )),
            );
        }

        if let Some(node_name) = &config.node_ids.dtn {
            // Add the Admin Endpoint EID itself
            add_pattern(
                bpv7::Eid::Dtn {
                    node_name: node_name.clone(),
                    demux: [].into(),
                }
                .into(),
                RibAction::AdminEndpoint,
            );

            add_pattern(
                format!("dtn://{node_name}/**").parse().unwrap(),
                RibAction::Drop(Some(
                    bpv7::StatusReportReasonCode::DestinationEndpointIDUnavailable,
                )),
            );
        }

        Arc::new(Self {
            routes: RwLock::new(routes),
            cancellable_waits: Mutex::default(),
        })
    }

    pub async fn add(
        &self,
        pattern: eid_pattern::EidPattern,
        source: String,
        action: RibAction,
        priority: u32,
    ) {
        info!("Adding route {pattern} => {action:?}, priority {priority}, source '{source}'");

        {
            self.routes.write().await.insert(
                pattern.clone(),
                Entry {
                    pattern: pattern.clone(),
                    source,
                    action,
                    priority,
                },
            )
        }

        // Wake all waiters
        self.wake(pattern.into()).await
    }

    pub async fn add_forward(
        &self,
        pattern: eid_pattern::EidPattern,
        cla_name: &str,
        cla: Arc<cla_registry::Cla>,
    ) {
        self.add(pattern, cla_name.to_string(), RibAction::Forward(cla), 0)
            .await
    }

    pub async fn add_local(
        &self,
        pattern: eid_pattern::EidPattern,
        service: Arc<service_registry::Service>,
    ) {
        self.add(pattern, LOCAL_SOURCE.clone(), RibAction::Local(service), 0)
            .await
    }

    pub async fn remove(
        &self,
        pattern: &eid_pattern::EidPattern,
        source: &str,
        action: &routes::Action,
        priority: u32,
    ) -> bool {
        let v = {
            self.routes.write().await.remove_if(pattern, |e| {
                if &e.pattern == pattern && e.source == source && e.priority == priority {
                    match (action, &e.action) {
                        (routes::Action::Drop(r1), RibAction::Drop(r2)) => r1 == r2,
                        (routes::Action::Via(eid1), RibAction::Via(eid2)) => eid1 == eid2,
                        (routes::Action::Store(until1), RibAction::Store(until2)) => {
                            until1 == until2
                        }
                        _ => false,
                    }
                } else {
                    false
                }
            })
        };

        for v in &v {
            info!(
                "Removed route {pattern} => {:?}, priority {priority}, source '{source}'",
                v.action
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

    pub async fn remove_forward(&self, pattern: &eid_pattern::EidPattern, cla_name: &str) -> bool {
        let v = {
            self.routes.write().await.remove_if(pattern, |e| {
                &e.pattern == pattern
                    && e.source == cla_name
                    && e.priority == 0
                    && matches!(&e.action, RibAction::Forward(..))
            })
        };

        for v in &v {
            info!(
                "Removed route {pattern} => {:?}, priority 0, source '{cla_name}'",
                v.action
            )
        }
        !v.is_empty()
    }

    pub async fn remove_local(
        &self,
        pattern: &eid_pattern::EidPattern,
        service: &service_registry::Service,
    ) -> bool {
        let v = self.routes.write().await.remove_if(pattern, |e| {
            if &e.pattern == pattern && e.source == *LOCAL_SOURCE && e.priority == 0 {
                if let RibAction::Local(e_service) = &e.action {
                    e_service.as_ref() == service
                } else {
                    false
                }
            } else {
                false
            }
        });

        for v in &v {
            info!(
                "Removed route {pattern} => {:?}, priority 0, source '{}'",
                v.action, *LOCAL_SOURCE
            )
        }
        !v.is_empty()
    }

    pub async fn find(
        &self,
        to: &bpv7::Eid,
    ) -> Result<FindResult, Option<bpv7::StatusReportReasonCode>> {
        let mut result = {
            let routes = self.routes.read().await;
            find_recurse(&routes, to, &mut HashSet::new())?
        };

        if let FindResult::Forward(clas, _) = &mut result {
            if clas.len() > 1 {
                // For ECMP, we need a random order
                clas.shuffle(&mut rand::rng());
            }
        }

        Ok(result)
    }

    pub async fn wait_for_route(
        &self,
        to: &bpv7::Eid,
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

    async fn wake(&self, pattern: eid_pattern::EidPatternSet) {
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

#[instrument(skip(routes, trail))]
fn find_recurse<'a>(
    routes: &'a eid_pattern::EidPatternMap<Entry>,
    to: &'a bpv7::Eid,
    trail: &mut HashSet<&'a bpv7::Eid>,
) -> Result<FindResult, Option<bpv7::StatusReportReasonCode>> {
    let mut result = FindResult::default();

    // Recursion check
    if !trail.insert(to) {
        warn!("Recursive route {to} found!");
        return Err(Some(
            bpv7::StatusReportReasonCode::NoKnownRouteToDestinationFromHere,
        ));
    }

    let mut entries = routes.find(to);

    // Sort the entries
    entries.sort();

    let mut priority = None;
    for entry in entries {
        // Ensure we only look at lowest priority values
        if let Some(priority) = priority {
            if entry.priority > priority {
                break;
            }
        } else {
            priority = Some(entry.priority);
        }

        match entry.action {
            RibAction::AdminEndpoint => {
                result = FindResult::AdminEndpoint;
                break;
            }
            RibAction::Local(ref service) => {
                result = FindResult::Deliver(service.clone());
                break;
            }
            RibAction::Forward(ref cla) => {
                let FindResult::Forward(clas, _) = &mut result else {
                    panic!("Mismatch in FindResult");
                };
                clas.push(cla.clone());
            }
            RibAction::Via(ref via) => {
                let sub_result = find_recurse(routes, via, trail)?;
                let FindResult::Forward(sub_clas, sub_until) = sub_result else {
                    result = sub_result;
                    break;
                };

                let FindResult::Forward(clas, until) = &mut result else {
                    panic!("Mismatch in FindResult");
                };

                clas.extend(sub_clas);

                if let Some(sub_until) = sub_until {
                    if let Some(until) = until {
                        if sub_until < *until {
                            *until = sub_until
                        }
                    } else {
                        *until = Some(sub_until)
                    }
                }
            }
            RibAction::Store(sub_until) => {
                let FindResult::Forward(_, until) = &mut result else {
                    panic!("Mismatch in FindResult");
                };

                if sub_until >= time::OffsetDateTime::now_utc() {
                    // Don't override a Store found with Via
                    if until.is_none() {
                        *until = Some(sub_until);
                    }

                    // The sort ensures that this is the shortest wait and have processed everything else relevant already
                    break;
                }
            }
            RibAction::Drop(reason) => {
                // Drop trumps everything else
                return Err(reason);
            }
        }

        trail.remove(to);
    }
    Ok(result)
}
