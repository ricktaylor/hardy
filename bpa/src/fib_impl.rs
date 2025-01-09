use super::*;
use rand::prelude::*;
use std::collections::HashSet;
use tokio::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Neighbour {
    pub addr: Option<Box<[u8]>>,
    pub cla: Arc<cla_registry::Cla>,
}

impl std::fmt::Display for Neighbour {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{:02x?}", self.cla, self.addr)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Action {
    Drop(Option<bpv7::StatusReportReasonCode>), // Drop the bundle
    Forward(Neighbour),                         // Forward to neighbour via CLA
    Via(bpv7::Eid),                             // Recursive lookup
    Store(time::OffsetDateTime),                // Wait for later availability
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Drop(reason) => {
                if let Some(reason) = reason {
                    write!(f, "drop({:?})", reason)
                } else {
                    write!(f, "drop")
                }
            }
            Self::Forward(c) => write!(f, "forward {}", c),
            Self::Via(eid) => write!(f, "via {eid}"),
            Self::Store(until) => write!(f, "Wait until {until}"),
        }
    }
}

impl From<&fib::Action> for Action {
    fn from(value: &fib::Action) -> Self {
        match value {
            fib::Action::Drop(reason_code) => Self::Drop(*reason_code),
            fib::Action::Via(eid) => Self::Via(eid.clone()),
            fib::Action::Store(until) => Self::Store(*until),
        }
    }
}

pub struct ForwardAction {
    pub clas: Vec<Neighbour>, // Available endpoints for forwarding
    pub until: Option<time::OffsetDateTime>, // Timestamp of next forwarding opportunity
}

type ForwardResult = Result<ForwardAction, Option<bpv7::StatusReportReasonCode>>;

type TableKey = String;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TableEntry {
    priority: u32,
    action: Action,
}

type Table = bpv7::EidPatternMap<TableKey, Vec<TableEntry>>;

pub struct Fib {
    entries: RwLock<Table>,
}

impl Fib {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Table::new()),
        }
    }

    #[instrument(skip_all)]
    pub async fn add(
        &self,
        id: &str,
        pattern: &bpv7::EidPattern,
        action: &fib::Action,
        priority: u32,
    ) -> fib::Result<()> {
        let id = id.to_string();

        info!("Adding route {pattern} => {action}, priority {priority}, source '{id}'");

        let entry = TableEntry {
            priority,
            action: action.into(),
        };

        let mut entries = self.entries.write().await;
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
    pub async fn remove(&self, id: &str, pattern: &bpv7::EidPattern) -> usize {
        self.entries
            .write()
            .await
            .remove(pattern, id)
            .inspect(|v| {
                for e in v {
                    info!(
                        "Removed route {pattern} => {}, priority {}, source '{id}'",
                        e.action, e.priority
                    );
                }
            })
            .map_or(0, |v| v.len())
    }

    #[instrument(skip(self))]
    pub async fn find(&self, to: &bpv7::Eid) -> ForwardResult {
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

    pub async fn add_neighbour(
        &self,
        destination: &bpv7::Eid,
        addr: Option<&[u8]>,
        priority: u32,
        cla: Arc<cla_registry::Cla>,
    ) -> cla::Result<()> {
        let id = cla.to_string();
        let neighbour = Neighbour {
            addr: addr.map(Into::into),
            cla,
        };

        info!("Adding neighbour {destination} => {neighbour}, priority {priority}");

        let entry = TableEntry {
            priority,
            action: Action::Forward(neighbour),
        };
        let pattern = destination.clone().into();

        let mut entries = self.entries.write().await;
        if let Some(mut prev) = entries.insert(&pattern, id.clone(), vec![entry.clone()]) {
            // We have previous - de-dedup
            if prev.binary_search(&entry).is_err() {
                prev.push(entry);
            }
            entries.insert(&pattern, id, prev);
        }
        Ok(())
    }

    pub async fn remove_neighbour(&self, cla: &Arc<cla_registry::Cla>, destination: &bpv7::Eid) {
        let id = cla.to_string();
        let pattern = destination.clone().into();

        self.entries
            .write()
            .await
            .remove(&pattern, &id)
            .inspect(|v| {
                for e in v {
                    info!(
                        "Removed neighbour {pattern} => {}, priority {}",
                        e.action, e.priority
                    );
                }
            });
    }
}

#[instrument(skip(table, trail))]
fn find_recurse(table: &Table, to: &bpv7::Eid, trail: &mut HashSet<bpv7::Eid>) -> ForwardResult {
    // TODO: We currently pick the first Drop action we find, and do not tie-break on reason...

    let mut new_action = ForwardAction {
        clas: Vec::new(),
        until: None,
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
                    new_action.until = match (new_action.until, action.until) {
                        (None, Some(_)) => action.until,
                        (_, None) => new_action.until,
                        (Some(new_until), Some(current_until)) => {
                            Some(new_until.min(current_until))
                        }
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
                Action::Store(until) => {
                    // Check we don't have a deadline in the past
                    if until >= time::OffsetDateTime::now_utc() {
                        new_action.until = match new_action.until {
                            None => Some(until),
                            Some(new_until) if new_until > until => Some(until),
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
