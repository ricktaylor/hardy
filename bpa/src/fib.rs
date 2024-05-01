use super::*;
use base64::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};

#[derive(Clone, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub enum Entry {
    Cla {
        protocol: String,
        address: Vec<u8>,
    },
    Ipn2 {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Ipn3 {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Dtn {
        node_name: String,
        demux: String,
    },
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Entry::Cla { protocol, address } => {
                write!(f, "{protocol}: {}", BASE64_STANDARD_NO_PAD.encode(address))
            }
            Entry::Ipn2 {
                allocator_id: 0,
                node_number,
                service_number,
            }
            | Entry::Ipn3 {
                allocator_id: 0,
                node_number,
                service_number,
            } => write!(f, "ipn:{node_number}.{service_number}"),
            Entry::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            }
            | Entry::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => write!(f, "ipn:{allocator_id}.{node_number}.{service_number}"),
            Entry::Dtn { node_name, demux } => write!(f, "dtn://{node_name}/{demux}"),
        }
    }
}

impl TryFrom<bundle::Eid> for Entry {
    type Error = anyhow::Error;

    fn try_from(value: bundle::Eid) -> Result<Self, Self::Error> {
        match value {
            bundle::Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            } => Ok(Self::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            }),
            bundle::Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => Ok(Self::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            }),
            bundle::Eid::Dtn { node_name, demux } => Ok(Self::Dtn { node_name, demux }),
            bundle::Eid::Null | bundle::Eid::LocalNode { service_number: _ } => {
                Err(anyhow!("Invalid FIB entry EID: {value}"))
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Drop,
    Forward(String), // Forward to CLA by token
    Via(Entry),      // Recursive lookup
    Store,
}

type Table = BTreeMap<Entry, Vec<Action>>;

#[derive(Clone)]
pub struct Fib {
    actions: Arc<RwLock<Table>>,
}

impl Fib {
    pub fn new(config: &config::Config) -> Option<Self> {
        if settings::get_with_default(config, "forwarding", true)
            .log_expect("Invalid 'forwarding' value in configuration")
        {
            Some(Self {
                actions: Default::default(),
            })
        } else {
            None
        }
    }

    pub fn add_action(&self, to: Entry, action: Action) {
        let mut table = self
            .actions
            .write()
            .log_expect("Failed to write-lock actions mutex");

        if let Some(actions) = table.get_mut(&to) {
            if actions.binary_search(&action).is_err() {
                actions.push(action);
                actions.sort();
            }
        } else {
            table.insert(to, vec![action]);
        }
    }

    pub fn lookup(&self, to: &Entry) -> Vec<Action> {
        let mut actions = {
            // Scope lock
            let table = self
                .actions
                .read()
                .log_expect("Failed to read-lock actions mutex");

            lookup_recurse(
                &table,
                table.get(to).unwrap_or(&Vec::new()),
                &mut HashSet::new(),
                0,
            )
        };

        // Sort by precedence
        actions.sort_unstable_by(action_precedence);

        // De-duplicate using the same sort order
        actions.dedup_by(|a1, a2| action_precedence(a1, a2).is_eq());

        // Remove the depth component
        actions.into_iter().map(|(_, a)| a).collect()
    }
}

fn action_precedence(a1: &(usize, Action), a2: &(usize, Action)) -> std::cmp::Ordering {
    match (a1, a2) {
        ((d1, Action::Forward(f1)), (d2, Action::Forward(f2))) => {
            /* Account for depth first */
            d1.cmp(d2).then(f1.cmp(f2))
        }
        ((d1, Action::Via(e1)), (d2, Action::Via(e2))) => {
            /* Account for depth first */
            d1.cmp(d2).then(e1.cmp(e2))
        }
        ((_, a1), (_, a2)) => {
            /* Depth is irrelevant */
            a1.cmp(a2)
        }
    }
}

fn lookup_recurse<'a>(
    table: &'a Table,
    actions: &'a [Action],
    trail: &mut HashSet<&'a Entry>,
    depth: usize,
) -> Vec<(usize, Action)> {
    let mut new_actions = Vec::new();
    for action in actions {
        if let Action::Via(via) = action {
            // Check for recursive Via
            if trail.insert(via) {
                if let Some(actions) = table.get(via) {
                    new_actions.extend(lookup_recurse(table, actions, trail, depth + 1));
                } else if let Entry::Cla {
                    protocol: _,
                    address: _,
                } = via
                {
                    // We allow CLA Via's to remain
                    new_actions.push((depth, action.clone()));
                }
                trail.remove(via);
            }
        } else {
            new_actions.push((depth, action.clone()))
        }
    }
    new_actions
}
