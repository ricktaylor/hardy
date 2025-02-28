use super::*;
use bpv7::Eid;
use rand::Rng;
use std::collections::HashSet;

#[derive(Debug)]
pub struct AdminEndpoints(HashSet<Eid>);

impl AdminEndpoints {
    pub fn new(eids: &[Eid]) -> Self {
        let mut h = HashSet::new();
        for eid in eids {
            if let Ok(eid) = eid.try_into_admin_endpoint() {
                match eid {
                    Eid::LegacyIpn { .. } | Eid::Ipn { .. } | Eid::Dtn { .. } => {
                        h.insert(eid);
                    }
                    _ => {}
                }
            }
        }
        if h.is_empty() {
            let mut rng = rand::rng();
            h.insert(Eid::Ipn {
                allocator_id: rng.random_range(0x40000000..0x80000000),
                node_number: rng.random_range(1..=u32::MAX),
                service_number: 0,
            });
        }
        Self(h)
    }

    pub fn contains(&self, eid: &Eid) -> bool {
        self.0.contains(eid)
    }

    pub fn get_admin_endpoint(&self, destination: &Eid) -> &Eid {
        self.0
            .iter()
            .filter(|eid| {
                matches!(
                    (eid, destination),
                    (Eid::LegacyIpn { .. }, Eid::LegacyIpn { .. })
                        | (Eid::LegacyIpn { .. }, Eid::Ipn { .. })
                        | (Eid::Ipn { .. }, Eid::LegacyIpn { .. })
                        | (Eid::Ipn { .. }, Eid::Ipn { .. })
                        | (Eid::Dtn { .. }, Eid::Dtn { .. })
                )
            })
            .chain(self.0.iter())
            .next()
            .unwrap()
    }

    pub fn is_local_service(&self, eid: &Eid) -> bool {
        match eid.try_into_admin_endpoint() {
            Ok(eid) => self.0.contains(&eid),
            Err(_) => false,
        }
    }

    pub fn dtn_node_id(&self) -> Option<&str> {
        self.0
            .iter()
            .filter_map(|eid| match eid {
                Eid::Dtn { node_name, .. } => Some(node_name.as_ref()),
                _ => None,
            })
            .next()
    }

    pub fn ipn_node_id(&self) -> Option<(u32, u32)> {
        self.0
            .iter()
            .filter_map(|eid| match eid {
                Eid::LegacyIpn {
                    allocator_id,
                    node_number,
                    ..
                }
                | Eid::Ipn {
                    allocator_id,
                    node_number,
                    ..
                } => Some((*allocator_id, *node_number)),
                _ => None,
            })
            .next()
    }
}
