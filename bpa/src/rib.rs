use super::*;
use rand::prelude::*;
use std::collections::{HashMap, HashSet};
use tokio::sync::{Mutex, RwLock};

#[derive(Default)]
pub struct ForwardAction {
    pub clas: Vec<String>,                   // Available endpoints for forwarding
    pub until: Option<time::OffsetDateTime>, // Timestamp of next forwarding opportunity
}

type ForwardResult = Result<ForwardAction, Option<bpv7::StatusReportReasonCode>>;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, std::hash::Hash)]
struct Entry {
    priority: u32,
    action: routes::Action,
    source: String,
}

#[derive(Debug)]
pub struct Rib {
    routes: RwLock<eid_pattern::EidPatternMap<Entry>>,
    cancellable_waits: Mutex<HashMap<bpv7::Eid, tokio_util::sync::CancellationToken>>,
}

impl Rib {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            routes: RwLock::new(eid_pattern::EidPatternMap::new()),
            cancellable_waits: Mutex::new(HashMap::new()),
        })
    }

    pub async fn add(
        &self,
        pattern: eid_pattern::EidPattern,
        source: String,
        action: routes::Action,
        priority: u32,
    ) {
        info!("Adding route {pattern} => {action}, priority {priority}, source '{source}'");

        {
            self.routes.write().await.insert(
                pattern.clone(),
                Entry {
                    source,
                    action,
                    priority,
                },
            )
        }

        // Wake all waiters
        self.wake(pattern.into()).await
    }

    pub async fn add_forward(&self, pattern: eid_pattern::EidPattern, cla_ident: &str) {
        self.add(pattern, cla_ident.to_string(), routes::Action::Forward, 0)
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
                e.source == source && e.priority == priority && &e.action == action
            })
        };

        for v in &v {
            info!(
                "Removed route {pattern} => {}, priority {priority}, source '{source}'",
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

    pub async fn remove_forward(&self, pattern: &eid_pattern::EidPattern, cla_ident: &str) -> bool {
        let v = {
            self.routes.write().await.remove_if(pattern, |e| {
                e.source == cla_ident
                    && e.priority == 0
                    && matches!(&e.action, routes::Action::Forward)
            })
        };

        for v in &v {
            info!(
                "Removed route {pattern} => {}, priority 0, source '{cla_ident}'",
                v.action
            )
        }
        !v.is_empty()
    }

    pub async fn find(&self, to: &bpv7::Eid) -> ForwardResult {
        let mut result = {
            let routes = self.routes.read().await;
            find_recurse(&routes, to, &mut HashSet::new())?
        };

        // For ECMP, we need a random order
        result.clas.shuffle(&mut rand::rng());

        Ok(result)
    }

    pub async fn wait_for_route(
        &self,
        to: &bpv7::Eid,
        duration: std::time::Duration,
        cancel_token: &tokio_util::sync::CancellationToken,
    ) {
        let token = {
            self.cancellable_waits
                .lock()
                .await
                .entry(to.clone())
                .or_insert(cancel_token.child_token())
                .clone()
        };

        // Wait a bit
        let timer = tokio::time::sleep(duration);
        tokio::pin!(timer);

        tokio::select! {
            () = &mut timer => {},
            _ = token.cancelled() => {
                // Remove the token from the map
                self.cancellable_waits
                    .lock()
                    .await.remove(to);
            }
        }
    }

    async fn wake(&self, pattern: eid_pattern::EidPatternSet) {
        let tokens = {
            self.cancellable_waits
                .lock()
                .await
                .extract_if(|eid, _| pattern.contains(eid))
                .map(|(_, token)| token)
                .collect::<Vec<_>>()
        };
        for token in tokens {
            token.cancel();
        }
    }
}

#[instrument(skip(routes, trail))]
fn find_recurse<'a>(
    routes: &'a eid_pattern::EidPatternMap<Entry>,
    to: &'a bpv7::Eid,
    trail: &mut HashSet<&'a bpv7::Eid>,
) -> ForwardResult {
    let mut result = ForwardAction::default();

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
            routes::Action::Drop(reason) => {
                // Drop trumps everything else
                return Err(reason);
            }
            routes::Action::Forward => {
                result.clas.push(entry.source.clone());
            }
            routes::Action::Via(ref via) => {
                let sub_result = find_recurse(routes, via, trail)?;
                if !sub_result.clas.is_empty() {
                    result.clas.extend(sub_result.clas);
                }

                if let Some(until) = sub_result.until {
                    if let Some(s_until) = result.until {
                        if until < s_until {
                            result.until = Some(until);
                        }
                    } else {
                        result.until = Some(until);
                    }
                }
            }
            routes::Action::Store(until) => {
                // Check we don't have a deadline in the past
                if until >= time::OffsetDateTime::now_utc() {
                    if let Some(s_until) = result.until {
                        if until < s_until {
                            result.until = Some(until);
                        }
                    } else {
                        result.until = Some(until);
                    }

                    // The sort ensures that this is the shortest wait and have processed everything else relevant already
                    break;
                }
            }
        }

        trail.remove(to);
    }
    Ok(result)
}
