use super::*;
use base64::prelude::*;
use rand::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Destination {
    Cla(ingress::ClaAddress),
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

impl std::fmt::Display for Destination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Destination::Cla(a) => {
                write!(
                    f,
                    "{}: {}/{}",
                    a.name,
                    a.protocol,
                    BASE64_STANDARD_NO_PAD.encode(&a.address)
                )
            }
            Destination::Ipn2 {
                allocator_id: 0,
                node_number,
                service_number,
            }
            | Destination::Ipn3 {
                allocator_id: 0,
                node_number,
                service_number,
            } => write!(f, "ipn:{node_number}.{service_number}"),
            Destination::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            }
            | Destination::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => write!(f, "ipn:{allocator_id}.{node_number}.{service_number}"),
            Destination::Dtn { node_name, demux } => write!(f, "dtn://{node_name}/{demux}"),
        }
    }
}

impl TryFrom<bundle::Eid> for Destination {
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

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Drop(Option<bundle::StatusReportReasonCode>), // Drop the bundle
    Wait(time::OffsetDateTime),                   // Wait for later availability
    Forward { protocol: String, address: Vec<u8> }, // Forward to CLA by protocol + address
    Via(Destination),                             // Recursive lookup
}

#[derive(Clone)]
pub enum ForwardAction {
    Drop(Option<bundle::StatusReportReasonCode>), // Drop the bundle
    Wait(time::OffsetDateTime),                   // Wait for later availability
    Forward(ingress::ClaAddress),                 // Forward to CLA by name
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct TableEntry {
    priority: u32,
    action: Action,
}

type Table = HashMap<Destination, Vec<TableEntry>>;

#[derive(Clone)]
pub struct Fib {
    entries: Arc<RwLock<Table>>,
}

impl Fib {
    pub fn new(config: &config::Config) -> Option<Self> {
        if settings::get_with_default(config, "forwarding", true)
            .log_expect("Invalid 'forwarding' value in configuration")
        {
            Some(Self {
                entries: Default::default(),
            })
        } else {
            None
        }
    }

    #[instrument(skip(self))]
    pub fn add(&self, to: Destination, priority: u32, action: Action) -> Result<(), anyhow::Error> {
        // Validate CLA actions
        if let Action::Forward {
            protocol,
            address: _,
        } = &action
        {
            if let Destination::Cla(a) = &to {
                if &a.protocol != protocol {
                    return Err(anyhow!(
                        "Must forward CLA addresses to CLAs of the same protocol"
                    ));
                }
            } else {
                return Err(anyhow!("Must forward CLA addresses to CLAs"));
            };
        }

        let mut table = self
            .entries
            .write()
            .log_expect("Failed to write-lock entries mutex");

        let entry = TableEntry { priority, action };
        if let Some(entries) = table.get_mut(&to) {
            if entries.binary_search(&entry).is_err() {
                entries.push(entry);
                entries.sort();
            }
        } else {
            table.insert(to, vec![entry]);
        }
        Ok(())
    }

    #[instrument(skip(self))]
    pub fn find(&self, to: &Destination) -> Vec<ForwardAction> {
        let table = self
            .entries
            .read()
            .log_expect("Failed to read-lock entries mutex");

        let mut actions = find_recurse(&table, to, &mut HashSet::new());
        if actions.len() > 1 {
            // For ECMP, we need a random order
            actions.shuffle(&mut rand::thread_rng());
        }
        actions
    }
}

#[instrument(skip(table, trail))]
fn find_recurse<'a>(
    table: &'a Table,
    to: &'a Destination,
    trail: &mut HashSet<&'a Destination>,
) -> Vec<ForwardAction> {
    let mut new_entries = Vec::new();
    if trail.insert(to) {
        if let Some(entries) = table.get(to) {
            let mut priority = None;
            for entry in entries {
                // Ensure we equal priority if we have multiple actions (ECMP)
                if let Some(priority) = priority {
                    if priority < entry.priority {
                        break;
                    }
                }

                match &entry.action {
                    Action::Via(via) => {
                        let entries = find_recurse(table, via, trail);
                        match entries.first() {
                            Some(ForwardAction::Drop(_)) => {
                                // Drop trumps everything else
                                return entries;
                            }
                            Some(_) => new_entries.extend(entries),
                            None => {}
                        }
                    }
                    Action::Forward {
                        protocol,
                        address: _,
                    } => {
                        if let Destination::Cla(a) = &to {
                            if &a.protocol == protocol {
                                new_entries.push(ForwardAction::Forward(a.clone()))
                            }
                        }
                    }
                    Action::Drop(reason) => {
                        // Drop trumps everything else
                        return vec![ForwardAction::Drop(*reason)];
                    }
                    Action::Wait(until) => {
                        // Check we don't have a deadline in the past
                        if *until >= time::OffsetDateTime::now_utc() {
                            new_entries.push(ForwardAction::Wait(*until))
                        }
                    }
                }

                if !new_entries.is_empty() {
                    priority = Some(entry.priority);
                }
            }
        }
        trail.remove(to);
    }

    // Remove any Wait actions, and remember the closest deadline
    let mut wait = None;
    new_entries = new_entries
        .into_iter()
        .filter_map(|a| match a {
            ForwardAction::Wait(until) => {
                wait = wait.map_or(
                    Some(until),
                    |w| {
                        if until < w {
                            Some(until)
                        } else {
                            Some(w)
                        }
                    },
                );
                None
            }
            a => Some(a),
        })
        .collect();

    // If we have no Forwarding actions, return the closest Wait action
    if new_entries.is_empty() {
        if let Some(until) = wait {
            new_entries.push(ForwardAction::Wait(until))
        }
    }
    new_entries
}
