use super::*;
use hardy_bpv7::eid::Eid;
use rand::Rng;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Node Ids must not be LocalNode")]
    LocalNode,

    #[error("Node Ids must not be the Null Endpoint")]
    NullEndpoint,

    #[error("Administrative endpoints must not have a dtn demux part")]
    DtnWithDemux,

    #[error("Multiple ipn scheme Node Ids")]
    MultipleIpnNodeIds,

    #[error("Multiple dtn scheme Node Ids")]
    MultipleDtnNodeIds,

    #[error(transparent)]
    InvalidEid(#[from] hardy_bpv7::eid::Error),
}

#[derive(Debug, Clone)]
pub struct NodeIds {
    pub(crate) ipn: Option<(u32, u32)>,
    pub(crate) dtn: Option<Box<str>>,
}

impl NodeIds {
    pub(crate) fn get_admin_endpoint(&self, destination: &Eid) -> Eid {
        match (destination, &self.ipn, &self.dtn) {
            (Eid::LegacyIpn { .. }, Some((allocator_id, node_number)), _) => Eid::LegacyIpn {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: 0,
            },
            (Eid::Dtn { .. }, _, Some(node_name)) | (_, None, Some(node_name)) => Eid::Dtn {
                node_name: node_name.clone(),
                demux: [].into(),
            },
            (_, Some((allocator_id, node_number)), _) => Eid::Ipn {
                allocator_id: *allocator_id,
                node_number: *node_number,
                service_number: 0,
            },
            (_, None, None) => unreachable!(),
        }
    }

    pub(crate) fn contains(&self, eid: &Eid) -> bool {
        match (eid, &self.ipn, &self.dtn) {
            (Eid::LocalNode { service_number }, Some(_), _) => service_number == &0,
            (
                Eid::LegacyIpn {
                    allocator_id,
                    node_number,
                    service_number,
                },
                Some((a, n)),
                _,
            )
            | (
                Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number,
                },
                Some((a, n)),
                _,
            ) => allocator_id == a && node_number == n && service_number == &0,
            (Eid::Dtn { node_name, demux }, _, Some(n)) => node_name == n && demux.is_empty(),
            _ => false,
        }
    }
}

impl Default for NodeIds {
    fn default() -> Self {
        let mut rng = rand::rng();
        Self {
            ipn: Some((
                rng.random_range(0x40000000..0x80000000),
                rng.random_range(1..=u32::MAX),
            )),
            dtn: None,
        }
    }
}

impl From<&NodeIds> for Vec<Eid> {
    fn from(value: &NodeIds) -> Self {
        let mut v = Vec::new();
        if let Some((allocator_id, node_number)) = value.ipn {
            v.push(Eid::Ipn {
                allocator_id,
                node_number,
                service_number: 0,
            });
        }
        if let Some(node_name) = &value.dtn {
            v.push(Eid::Dtn {
                node_name: node_name.clone(),
                demux: [].into(),
            });
        }
        v
    }
}

impl TryFrom<&[Eid]> for NodeIds {
    type Error = Error;

    fn try_from(eids: &[Eid]) -> Result<Self, Self::Error> {
        let mut ipn = None;
        let mut dtn = None;
        for eid in eids {
            match eid {
                Eid::LegacyIpn {
                    allocator_id,
                    node_number,
                    service_number: 0,
                }
                | Eid::Ipn {
                    allocator_id,
                    node_number,
                    service_number: 0,
                } => {
                    if let Some((a, n)) = ipn {
                        if a != *allocator_id || n != *node_number {
                            return Err(Error::MultipleIpnNodeIds);
                        }
                    } else {
                        ipn = Some((*allocator_id, *node_number));
                    }
                }
                Eid::Dtn { node_name, demux } if demux.is_empty() => {
                    if let Some(n) = &dtn {
                        if n != node_name {
                            return Err(Error::MultipleDtnNodeIds);
                        }
                    } else {
                        dtn = Some(node_name.clone());
                    }
                }
                Eid::LegacyIpn {
                    allocator_id: _,
                    node_number: _,
                    service_number,
                }
                | Eid::Ipn {
                    allocator_id: _,
                    node_number: _,
                    service_number,
                } => {
                    return Err(hardy_bpv7::eid::Error::IpnInvalidServiceNumber(
                        *service_number as u64,
                    )
                    .into());
                }
                Eid::Dtn { .. } => return Err(Error::DtnWithDemux),
                Eid::Null => return Err(Error::NullEndpoint),
                Eid::LocalNode { .. } => {
                    return Err(Error::LocalNode);
                }
                Eid::Unknown { scheme, .. } => {
                    return Err(hardy_bpv7::eid::Error::UnsupportedScheme(*scheme).into());
                }
            }
        }
        Ok(Self { ipn, dtn })
    }
}

impl Serialize for NodeIds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match (&self.ipn, &self.dtn) {
            (None, None) => unreachable!(),
            (None, Some(node_name)) => serializer.serialize_str(
                Eid::Dtn {
                    node_name: node_name.clone(),
                    demux: [].into(),
                }
                .to_string()
                .as_str(),
            ),
            (Some((allocator_id, node_number)), None) => serializer.serialize_str(
                Eid::Ipn {
                    allocator_id: *allocator_id,
                    node_number: *node_number,
                    service_number: 0,
                }
                .to_string()
                .as_str(),
            ),
            (Some((allocator_id, node_number)), Some(node_name)) => serializer.collect_seq([
                Eid::Ipn {
                    allocator_id: *allocator_id,
                    node_number: *node_number,
                    service_number: 0,
                }
                .to_string(),
                Eid::Dtn {
                    node_name: node_name.clone(),
                    demux: [].into(),
                }
                .to_string(),
            ]),
        }
    }
}

impl<'de> Deserialize<'de> for NodeIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct AdminEndpointsVisitor;

        impl<'de> Visitor<'de> for AdminEndpointsVisitor {
            type Value = NodeIds;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a single EID or a sequence of EIDs")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                [value.parse().map_err(E::custom)?]
                    .as_slice()
                    .try_into()
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
                endpoints.as_slice().try_into().map_err(de::Error::custom)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(NodeIds::default())
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(NodeIds::default())
            }
        }

        deserializer.deserialize_any(AdminEndpointsVisitor)
    }
}
