use hardy_bpv7::eid::{DtnNodeId, Eid, IpnNodeId, NodeId};
use rand::RngExt;
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

    #[error("No IPN node ID configured")]
    NoIpnNodeId,

    #[error("No DTN node ID configured")]
    NoDtnNodeId,

    #[error(transparent)]
    InvalidEid(#[from] hardy_bpv7::eid::Error),
}

/// The BPA's configured node identifiers.
///
/// A BPA may operate with an `ipn`-scheme node ID, a `dtn`-scheme node ID,
/// or both. At least one must be present. These are used to derive
/// administrative endpoints and to identify the local node in routing.
///
/// When no explicit identifiers are provided, [`Default`] generates a
/// random IPN node ID in the private allocator range.
#[derive(Debug, Clone)]
pub struct NodeIds {
    pub(crate) ipn: Option<IpnNodeId>,
    pub(crate) dtn: Option<DtnNodeId>,
}

impl NodeIds {
    pub(crate) fn get_admin_endpoint(&self, destination: &Eid) -> Eid {
        match (destination, &self.ipn, &self.dtn) {
            (Eid::LegacyIpn { .. }, Some(node_id), _) => Eid::LegacyIpn {
                fqnn: *node_id,
                service_number: 0,
            },
            (Eid::Dtn { .. }, _, Some(node_id)) | (_, None, Some(node_id)) => {
                node_id.clone().into()
            }
            (_, Some(node_id), _) => (*node_id).into(),
            (_, None, None) => unreachable!("NodeIds require at least one scheme at construction"),
        }
    }

    /// If `eid` belongs to this node (IPN scheme), return the `LocalNode` form.
    /// Returns `None` if no conversion is needed (non-local or DTN EIDs).
    pub(crate) fn to_local_eid(&self, eid: &Eid) -> Option<Eid> {
        match eid {
            Eid::Ipn {
                fqnn,
                service_number,
            }
            | Eid::LegacyIpn {
                fqnn,
                service_number,
            } if Some(*fqnn) == self.ipn => Some(Eid::LocalNode(*service_number)),
            _ => None,
        }
    }

    /// Resolve a service identifier to a full EID using this node's identity.
    pub(crate) fn resolve_eid(&self, service_id: &hardy_bpv7::eid::Service) -> Result<Eid, Error> {
        match service_id {
            hardy_bpv7::eid::Service::Ipn(n) => Ok(Eid::Ipn {
                fqnn: self.ipn.ok_or(Error::NoIpnNodeId)?,
                service_number: *n,
            }),
            hardy_bpv7::eid::Service::Dtn(name) => Ok(Eid::Dtn {
                node_name: self.dtn.clone().ok_or(Error::NoDtnNodeId)?,
                service_name: name.clone(),
            }),
        }
    }
}

impl Default for NodeIds {
    fn default() -> Self {
        let mut rng = rand::rng();
        Self {
            ipn: Some(IpnNodeId {
                allocator_id: rng.random_range(0x40000000..0x80000000),
                node_number: rng.random_range(1..=u32::MAX),
            }),
            dtn: None,
        }
    }
}

impl From<&NodeIds> for Vec<NodeId> {
    fn from(value: &NodeIds) -> Self {
        let mut v = Vec::with_capacity(2);
        if let Some(node_id) = &value.ipn {
            v.push(NodeId::Ipn(*node_id));
        }
        if let Some(node_id) = &value.dtn {
            v.push(NodeId::Dtn(node_id.clone()));
        }
        v
    }
}

impl TryFrom<&[NodeId]> for NodeIds {
    type Error = Error;

    fn try_from(node_ids: &[NodeId]) -> Result<Self, Self::Error> {
        let mut ipn = None;
        let mut dtn = None;
        for node_id in node_ids {
            match node_id {
                NodeId::Ipn(node_id) => {
                    if let Some(existing) = &ipn
                        && node_id != existing
                    {
                        return Err(Error::MultipleIpnNodeIds);
                    }
                    ipn = Some(*node_id);
                }
                NodeId::Dtn(node_id) => {
                    if let Some(existing) = &dtn
                        && existing != node_id
                    {
                        return Err(Error::MultipleDtnNodeIds);
                    }
                    dtn = Some(node_id.clone());
                }
                NodeId::LocalNode => {
                    return Err(Error::LocalNode);
                }
            }
        }
        Ok(Self { ipn, dtn })
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for NodeIds {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match (&self.ipn, &self.dtn) {
            (None, None) => unreachable!("NodeIds requires at least one scheme at construction"),
            (None, Some(node_id)) => serializer.serialize_str(node_id.to_string().as_str()),
            (Some(node_id), None) => serializer.serialize_str(node_id.to_string().as_str()),
            (Some(ipn_node_id), Some(dtn_node_id)) => {
                serializer.collect_seq([ipn_node_id.to_string(), dtn_node_id.to_string()])
            }
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for NodeIds {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct AdminEndpointsVisitor;

        impl<'de> serde::de::Visitor<'de> for AdminEndpointsVisitor {
            type Value = NodeIds;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a single NodeId or a sequence of NodeIds")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                [value.parse().map_err(E::custom)?]
                    .as_slice()
                    .try_into()
                    .map_err(serde::de::Error::custom)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut endpoints = Vec::new();
                while let Some(eid) = seq.next_element::<String>()? {
                    endpoints.push(eid.parse().map_err(serde::de::Error::custom)?);
                }
                endpoints
                    .as_slice()
                    .try_into()
                    .map_err(serde::de::Error::custom)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(NodeIds::default())
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(NodeIds::default())
            }
        }

        deserializer.deserialize_any(AdminEndpointsVisitor)
    }
}

impl core::fmt::Display for NodeIds {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match (&self.ipn, &self.dtn) {
            (None, None) => write!(f, "[]"),
            (None, Some(node_id)) => write!(f, "[ {node_id} ]"),
            (Some(node_id), None) => write!(f, "[ {node_id} ]"),
            (Some(ipn_node_id), Some(dtn_node_id)) => write!(f, "[ {ipn_node_id}, {dtn_node_id} ]"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipn(alloc: u32, node: u32) -> NodeId {
        NodeId::Ipn(IpnNodeId {
            allocator_id: alloc,
            node_number: node,
        })
    }

    fn dtn(name: &str) -> NodeId {
        NodeId::Dtn(DtnNodeId {
            node_name: name.into(),
        })
    }

    // Two different IPN node IDs should be rejected.
    #[test]
    fn test_single_scheme_enforce() {
        let ids = [ipn(0, 1), ipn(0, 2)];
        let result = NodeIds::try_from(ids.as_slice());
        assert!(matches!(result, Err(Error::MultipleIpnNodeIds)));

        // Same IPN ID twice should be OK (idempotent)
        let ids = [ipn(0, 1), ipn(0, 1)];
        assert!(NodeIds::try_from(ids.as_slice()).is_ok());

        // Two different DTN node IDs should also be rejected
        let ids = [dtn("node-a"), dtn("node-b")];
        let result = NodeIds::try_from(ids.as_slice());
        assert!(matches!(result, Err(Error::MultipleDtnNodeIds)));
    }

    // LocalNode should be rejected.
    #[test]
    fn test_invalid_types() {
        let ids = [NodeId::LocalNode];
        let result = NodeIds::try_from(ids.as_slice());
        assert!(matches!(result, Err(Error::LocalNode)));

        // LocalNode alongside a valid ID should also be rejected
        let ids = [ipn(0, 1), NodeId::LocalNode];
        let result = NodeIds::try_from(ids.as_slice());
        assert!(matches!(result, Err(Error::LocalNode)));
    }

    #[test]
    fn test_to_local_eid_ipn() {
        let node_ids = NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: None,
        };

        // Local IPN EID should convert to LocalNode
        let eid = Eid::Ipn {
            fqnn: IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            },
            service_number: 42,
        };
        assert_eq!(node_ids.to_local_eid(&eid), Some(Eid::LocalNode(42)));

        // LegacyIpn should also convert
        let legacy = Eid::LegacyIpn {
            fqnn: IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            },
            service_number: 7,
        };
        assert_eq!(node_ids.to_local_eid(&legacy), Some(Eid::LocalNode(7)));

        // Non-local IPN should return None
        let remote = Eid::Ipn {
            fqnn: IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            },
            service_number: 42,
        };
        assert_eq!(node_ids.to_local_eid(&remote), None);
    }

    #[test]
    fn test_to_local_eid_dtn() {
        let node_ids = NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: Some(DtnNodeId {
                node_name: "mynode".into(),
            }),
        };

        // DTN EIDs should always return None (no LocalNode equivalent)
        let dtn_eid: Eid = "dtn://mynode/svc".parse().unwrap();
        assert_eq!(node_ids.to_local_eid(&dtn_eid), None);
    }

    #[test]
    fn test_to_local_eid_no_ipn() {
        let node_ids = NodeIds {
            ipn: None,
            dtn: Some(DtnNodeId {
                node_name: "mynode".into(),
            }),
        };

        // With no IPN node ID configured, all IPN EIDs return None
        let eid = Eid::Ipn {
            fqnn: IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            },
            service_number: 42,
        };
        assert_eq!(node_ids.to_local_eid(&eid), None);
    }

    // Admin EID for IPN destination should use the IPN node ID with service 0.
    #[test]
    fn test_admin_resolution_ipn() {
        let node_ids = NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: Some(DtnNodeId {
                node_name: "mynode".into(),
            }),
        };

        // 3-element IPN destination hits catch-all → Eid::Ipn admin endpoint
        let dest: Eid = "ipn:0.5.42".parse().unwrap();
        let admin = node_ids.get_admin_endpoint(&dest);
        assert_eq!(
            admin,
            Eid::Ipn {
                fqnn: IpnNodeId {
                    allocator_id: 0,
                    node_number: 1,
                },
                service_number: 0,
            }
        );

        // Legacy 2-element IPN destination → Eid::LegacyIpn admin endpoint
        let legacy_dest = Eid::LegacyIpn {
            fqnn: IpnNodeId {
                allocator_id: 0,
                node_number: 5,
            },
            service_number: 42,
        };
        let admin = node_ids.get_admin_endpoint(&legacy_dest);
        assert_eq!(
            admin,
            Eid::LegacyIpn {
                fqnn: IpnNodeId {
                    allocator_id: 0,
                    node_number: 1,
                },
                service_number: 0,
            }
        );
    }

    // Admin EID for DTN destination should use the DTN node ID.
    #[test]
    fn test_admin_resolution_dtn() {
        let node_ids = NodeIds {
            ipn: Some(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            dtn: Some(DtnNodeId {
                node_name: "mynode".into(),
            }),
        };

        // DTN destination should resolve to DTN admin endpoint
        let dest: Eid = "dtn://mynode/svc".parse().unwrap();
        let admin = node_ids.get_admin_endpoint(&dest);
        let expected: Eid = "dtn://mynode/".parse().unwrap();
        assert_eq!(admin, expected);

        // With only DTN node ID, any destination should resolve to DTN
        let dtn_only = NodeIds {
            ipn: None,
            dtn: Some(DtnNodeId {
                node_name: "solo".into(),
            }),
        };
        let ipn_dest: Eid = "ipn:0.5.42".parse().unwrap();
        let admin = dtn_only.get_admin_endpoint(&ipn_dest);
        let expected: Eid = "dtn://solo/".parse().unwrap();
        assert_eq!(admin, expected);
    }
}
