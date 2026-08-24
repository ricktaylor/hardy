use hardy_bpa::node_ids::{Error, NodeIds};
use hardy_bpv7::eid::{DtnNodeId, IpnNodeId, NodeId};

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
