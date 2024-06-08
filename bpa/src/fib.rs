use super::*;
use rand::prelude::*;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use utils::settings;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Endpoint {
    pub handle: u32, // The CLA handle
                     // TODO: Metrics, e.g.: Bandwidth, Contact deadline
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Drop(Option<bundle::StatusReportReasonCode>), // Drop the bundle
    Forward(Endpoint),                            // Forward to CLA by Handle
    Via(bundle::Eid),                             // Recursive lookup
    Wait(time::OffsetDateTime),                   // Wait for later availability
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Action::Drop(reason) => {
                if let Some(reason) = reason {
                    write!(f, "drop({:?})", reason)
                } else {
                    write!(f, "drop")
                }
            }
            Action::Forward(c) => write!(f, "forward {}", c.handle),
            Action::Via(eid) => write!(f, "via {eid}"),
            Action::Wait(until) => write!(f, "Wait until {until}"),
        }
    }
}

pub struct ForwardAction {
    pub clas: Vec<Endpoint>,                // Available endpoints for forwarding
    pub wait: Option<time::OffsetDateTime>, // Timestamp of next forwarding opportunity
}

type ForwardResult = Result<ForwardAction, Option<bundle::StatusReportReasonCode>>;

type TableKey = String;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableEntry {
    pub priority: u32,
    pub action: Action,
}

type Table = bundle::EidPatternMap<TableKey, Vec<TableEntry>>;

#[derive(Default, Clone)]
pub struct Fib {
    entries: Arc<RwLock<Table>>,
}

impl Fib {
    pub fn new(config: &config::Config) -> Option<Self> {
        settings::get_with_default::<bool, _>(config, "forwarding", true)
            .trace_expect("Invalid 'forwarding' value in configuration")
            .then(Self::default)
    }

    #[instrument(skip_all)]
    pub async fn add(
        &self,
        id: String,
        pattern: &bundle::EidPattern,
        priority: u32,
        action: Action,
    ) -> Result<(), Error> {
        info!("Add route {pattern} => {action}, priority {priority}, source '{id}'");

        let mut entries = self.entries.write().await;
        let entry = TableEntry { priority, action };
        if let Some(mut prev) = entries.insert(pattern, id.clone(), vec![entry.clone()]) {
            // We have previous - de-dedup
            if prev.binary_search(&entry).is_err() {
                prev.push(entry);
            }
            entries.insert(pattern, id, prev);
        }
        Ok(())
    }

    #[instrument(skip_all)]
    pub async fn remove(&self, id: &str, pattern: &bundle::EidPattern) -> Option<Vec<TableEntry>> {
        info!("Remove route {pattern}, source '{id}'");

        self.entries.write().await.remove(pattern, id)
    }

    #[instrument(skip(self))]
    pub async fn find(&self, to: &bundle::Eid) -> ForwardResult {
        let mut action = {
            // Scope the lock
            let entries = self.entries.read().await;
            find_recurse(&entries, to, &mut HashSet::new())?
        };

        if action.clas.len() > 1 {
            // For ECMP, we need a random order
            action.clas.shuffle(&mut rand::thread_rng());
        }
        Ok(action)
    }
}

#[instrument(skip(table, trail))]
fn find_recurse(
    table: &Table,
    to: &bundle::Eid,
    trail: &mut HashSet<bundle::Eid>,
) -> ForwardResult {
    // TODO: We currently pick the first Drop action we find, and do not tie-break on reason...

    let mut new_action = ForwardAction {
        clas: Vec::new(),
        wait: None,
    };

    // Recursion check
    if trail.insert(to.clone()) {
        // Flatten and Filter on lowest priority
        // TODO: This is a fairly brutal binning by priority, keeping the lowest bin
        let mut priority = None;
        let mut entries = Vec::new();
        for entry in table.find(to).into_iter().flatten() {
            match priority {
                Some(lowest_priority) if lowest_priority < entry.priority => continue,
                Some(lowest_priority) if lowest_priority > entry.priority => entries.clear(),
                _ => {}
            }
            priority = Some(entry.priority);
            entries.push(entry.action.clone());
        }

        for action in entries {
            match action {
                Action::Via(via) => {
                    let action = find_recurse(table, &via, trail)?;
                    new_action.wait = match (new_action.wait, action.wait) {
                        (None, Some(_)) => action.wait,
                        (_, None) => new_action.wait,
                        (Some(wait), Some(until)) => Some(wait.min(until)),
                    };
                    new_action.clas.extend(action.clas)
                }
                Action::Forward(c) => {
                    new_action.clas.push(c);
                }
                Action::Drop(reason) => {
                    // Drop trumps everything else
                    return Err(reason);
                }
                Action::Wait(until) => {
                    // Check we don't have a deadline in the past
                    if until >= time::OffsetDateTime::now_utc() {
                        new_action.wait = match new_action.wait {
                            None => Some(until),
                            Some(wait) if wait > until => Some(until),
                            w => w,
                        };
                    }
                }
            }
        }
        trail.remove(to);
    }
    Ok(new_action)
}
