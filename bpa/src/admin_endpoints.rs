use super::*;
use rand::Rng;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Administrative endpoints must not be LocalNode")]
    LocalNode,

    #[error("Administrative endpoints must not be the Null Endpoint")]
    NullEndpoint,

    #[error("Administrative endpoints must not have a dtn demux part")]
    DtnWithDemux,

    #[error(transparent)]
    InvalidEid(#[from] bpv7::EidError),
}

#[derive(Debug, Clone)]
pub struct AdminEndpoints(Arc<HashSet<bpv7::Eid>>);

impl AdminEndpoints {
    pub fn init(eids: &[bpv7::Eid]) -> Result<Self, Error> {
        let mut h = HashSet::new();
        for eid in eids {
            let eid = match eid {
                bpv7::Eid::LegacyIpn {
                    allocator_id,
                    node_number,
                    service_number: 0,
                }
                | bpv7::Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number: 0,
                } => bpv7::Eid::Ipn {
                    allocator_id: *allocator_id,
                    node_number: *node_number,
                    service_number: 0,
                },
                bpv7::Eid::Dtn {
                    node_name: _,
                    demux,
                } if demux.is_empty() => eid.clone(),
                bpv7::Eid::LegacyIpn {
                    allocator_id: _,
                    node_number: _,
                    service_number,
                }
                | bpv7::Eid::Ipn {
                    allocator_id: _,
                    node_number: _,
                    service_number,
                } => {
                    return Err(
                        bpv7::EidError::IpnInvalidServiceNumber(*service_number as u64).into(),
                    );
                }
                bpv7::Eid::Dtn { .. } => return Err(Error::DtnWithDemux),
                bpv7::Eid::Null => return Err(Error::NullEndpoint),
                bpv7::Eid::LocalNode { .. } => {
                    return Err(Error::LocalNode);
                }
                bpv7::Eid::Unknown { scheme, .. } => {
                    return Err(bpv7::EidError::UnsupportedScheme(*scheme).into());
                }
            };
            h.insert(eid);
        }
        if h.is_empty() {
            Ok(Self::default())
        } else {
            Ok(Self(Arc::new(h)))
        }
    }

    pub(super) fn contains(&self, eid: &bpv7::Eid) -> bool {
        self.0.contains(eid)
    }

    pub(super) fn get_admin_endpoint(&self, destination: &bpv7::Eid) -> &bpv7::Eid {
        self.0
            .iter()
            .filter(|eid| {
                matches!(
                    (eid, destination),
                    (bpv7::Eid::Ipn { .. }, bpv7::Eid::LegacyIpn { .. })
                        | (bpv7::Eid::Ipn { .. }, bpv7::Eid::Ipn { .. })
                        | (bpv7::Eid::Dtn { .. }, bpv7::Eid::Dtn { .. })
                )
            })
            .chain(self.0.iter())
            .next()
            .unwrap()
    }

    pub(super) fn is_local_service(&self, eid: &bpv7::Eid) -> bool {
        match eid {
            bpv7::Eid::LocalNode { .. } => true,
            bpv7::Eid::LegacyIpn {
                allocator_id,
                node_number,
                service_number: _,
            } => self.0.contains(&bpv7::Eid::Ipn {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: 0,
            }),
            bpv7::Eid::Ipn {
                allocator_id: _,
                node_number: _,
                service_number: 0,
            } => self.0.contains(eid),
            bpv7::Eid::Ipn {
                allocator_id,
                node_number,
                service_number: _,
            } => self.0.contains(&bpv7::Eid::Ipn {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: 0,
            }),
            bpv7::Eid::Dtn {
                node_name,
                demux: _,
            } => self.0.contains(&bpv7::Eid::Dtn {
                node_name: node_name.clone(),
                demux: [].into(),
            }),
            _ => false,
        }
    }

    pub(super) fn dtn_node_id(&self) -> Option<&str> {
        self.0
            .iter()
            .filter_map(|eid| match eid {
                bpv7::Eid::Dtn { node_name, .. } => Some(node_name.as_ref()),
                _ => None,
            })
            .next()
    }

    pub(super) fn ipn_node_id(&self) -> Option<(u32, u32)> {
        self.0
            .iter()
            .filter_map(|eid| match eid {
                bpv7::Eid::LegacyIpn {
                    allocator_id,
                    node_number,
                    ..
                }
                | bpv7::Eid::Ipn {
                    allocator_id,
                    node_number,
                    ..
                } => Some((*allocator_id, *node_number)),
                _ => None,
            })
            .next()
    }
}

impl Default for AdminEndpoints {
    fn default() -> Self {
        let mut rng = rand::rng();
        let mut h = HashSet::new();
        h.insert(bpv7::Eid::Ipn {
            allocator_id: rng.random_range(0x40000000..0x80000000),
            node_number: rng.random_range(1..=u32::MAX),
            service_number: 0,
        });
        Self(Arc::new(h))
    }
}

impl Serialize for AdminEndpoints {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.0.len() == 1 {
            serializer.serialize_str(&self.0.iter().next().unwrap().to_string())
        } else {
            serializer.collect_seq(self.0.iter().map(|eid| eid.to_string()))
        }
    }
}

impl<'de> Deserialize<'de> for AdminEndpoints {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct AdminEndpointsVisitor;

        impl<'de> Visitor<'de> for AdminEndpointsVisitor {
            type Value = AdminEndpoints;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a single EID or a sequence of EIDs")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                AdminEndpoints::init(&[value.parse().map_err(E::custom)?])
                    .map_err(de::Error::custom)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut endpoints = Vec::new();
                while let Some(eid) = seq.next_element::<String>()? {
                    endpoints.push(eid.parse().map_err(de::Error::custom)?);
                }
                AdminEndpoints::init(&endpoints).map_err(de::Error::custom)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(AdminEndpoints::default())
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(AdminEndpoints::default())
            }
        }

        deserializer.deserialize_any(AdminEndpointsVisitor)
    }
}
