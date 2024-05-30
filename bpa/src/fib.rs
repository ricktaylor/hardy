use super::*;
use rand::prelude::*;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use utils::settings;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Drop(Option<bundle::StatusReportReasonCode>), // Drop the bundle
    Forward(u32),                                 // Forward to CLA by Handle
    Via(bundle::Eid),                             // Recursive lookup
    Wait(time::OffsetDateTime),                   // Wait for later availability
}

#[derive(Clone)]
pub enum ForwardAction {
    Drop(Option<bundle::StatusReportReasonCode>), // Drop the bundle
    Forward(Vec<u32>, Option<time::OffsetDateTime>), // Forward to CLA by Handle
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EidTableEntry {
    priority: u32,
    action: Action,
}

type EidTable = bundle::EidPatternMap<String, Vec<EidTableEntry>>;

#[derive(Default, Clone)]
pub struct Fib {
    entries: Arc<RwLock<EidTable>>,
}

impl Fib {
    pub fn new(config: &config::Config) -> Option<Self> {
        settings::get_with_default::<bool, _>(config, "forwarding", true)
            .trace_expect("Invalid 'forwarding' value in configuration")
            .then(Self::default)
    }

    #[instrument(skip(self))]
    pub async fn add(
        &self,
        id: String,
        pattern: &bundle::EidPattern,
        priority: u32,
        action: Action,
    ) -> Result<(), Error> {
        let mut entries = self.entries.write().await;
        let entry = EidTableEntry { priority, action };
        if let Some(mut prev) = entries.insert(pattern, id.clone(), vec![entry.clone()]) {
            // We have previous - de-dedup
            if prev.binary_search(&entry).is_err() {
                prev.push(entry);
            }
            entries.insert(pattern, id, prev);
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn find(&self, to: &bundle::Eid) -> ForwardAction {
        let r = {
            // Scope the lock
            let entries = self.entries.read().await;
            find_recurse(&entries, to, &mut HashSet::new())
        };

        match r {
            ForwardAction::Forward(mut v, until) if v.len() > 1 => {
                // For ECMP, we need a random order
                v.shuffle(&mut rand::thread_rng());
                ForwardAction::Forward(v, until)
            }
            r => r,
        }
    }
}

fn priority_subset<I, F1, F2, R>(iter: I, f1: F1, f2: F2) -> Vec<R>
where
    I: Iterator,
    F1: Fn(&I::Item) -> u32,
    F2: Fn(&I::Item) -> R,
{
    // This is a fairly brutal binning by priority, with 1 bin
    let mut lowest_priority = None;
    let mut entries = Vec::new();
    for i in iter {
        let p = f1(&i);
        match lowest_priority {
            Some(lowest_priority) if lowest_priority < p => continue,
            Some(lowest_priority) if lowest_priority > p => entries.clear(),
            _ => {}
        }
        lowest_priority = Some(p);
        entries.push(f2(&i));
    }
    entries
}

#[instrument(skip(table, trail))]
fn find_recurse(
    table: &EidTable,
    to: &bundle::Eid,
    trail: &mut HashSet<bundle::Eid>,
) -> ForwardAction {
    // TODO: We currently pick the first Drop action we find, and do not tie-break on reason...

    let mut new_entries = Vec::new();
    let mut wait = None;

    // Recursion check
    if trail.insert(to.clone()) {
        // Flatten and Filter on lowest priority
        let entries = priority_subset(
            table.find(to).into_iter().flatten(),
            |e| e.priority,
            |e| e.action.clone(),
        );
        for action in entries {
            match action {
                Action::Via(via) => match find_recurse(table, &via, trail) {
                    ForwardAction::Drop(reason) => return ForwardAction::Drop(reason),
                    ForwardAction::Forward(entries, until) => {
                        wait = match (wait, until) {
                            (None, Some(_)) => until,
                            (_, None) => wait,
                            (Some(wait), Some(until)) => Some(wait.min(until)),
                        };
                        new_entries.extend(entries)
                    }
                },
                Action::Forward(c) => {
                    new_entries.push(c);
                }
                Action::Drop(reason) => {
                    // Drop trumps everything else
                    return ForwardAction::Drop(reason);
                }
                Action::Wait(until) => {
                    // Check we don't have a deadline in the past
                    if until >= time::OffsetDateTime::now_utc() {
                        wait =
                            wait.map_or(Some(until), |w: time::OffsetDateTime| Some(w.min(until)));
                    }
                }
            }
        }
        trail.remove(to);
    }
    ForwardAction::Forward(new_entries, wait)
}
