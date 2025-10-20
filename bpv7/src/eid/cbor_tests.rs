use super::*;
use error::Error;
use hex_literal::hex;

#[test]
fn tests() {
    // Positive tests
    ipn_check(&hex!("82 02 82 01 01"), 0, 1, 1);
    ipn_check(&hex!("82 02 83 00 01 01"), 0, 1, 1);

    ipn_check_legacy(&hex!("82 02 82 1B 000EE868 00000001 01"), 977000, 1, 1);
    ipn_check(&hex!("82 02 83 1A 000EE868 01 01"), 977000, 1, 1);

    null_check(&hex!("82 02 82 00 00"));
    null_check(&hex!("82 02 83 00 00 00"));

    // From Stephan Havermans testing
    null_check(&hex!("82 02 82 00 01"));
    null_check(&hex!("82 02 83 00 00 01"));

    dtn_check(&hex!("82 01 67 2f2f6e6f64652f"), "node", "");
    dtn_check(
        &hex!("82 01 6f 2f2f6c6f6e676e6f64656e616d652f"),
        "longnodename",
        "",
    );
    dtn_check(
        &hex!("82 01 76 2f2f6c6f6e676e6f64656e616d652f73657276696365"),
        "longnodename",
        "service",
    );

    null_check(&hex!("82 01 64 6e6f6e65"));

    // Negative tests
    assert!(matches!(
        expect_error(&[]),
        Error::InvalidCBOR(hardy_cbor::decode::Error::NeedMoreData(1))
    ));
    assert!(matches!(
        expect_error(&hex!(
            "82 02 83 1B 0000000800000001 1B 0000000800000001 1B 0000000800000001"
        )),
        Error::IpnInvalidAllocatorId(_)
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 83 01 1B 0000000800000001 1B 0000000800000001")),
        Error::IpnInvalidNodeNumber(_)
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 83 01 01 1B 0000000800000001")),
        Error::IpnInvalidServiceNumber(_)
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 81 00")),
        Error::InvalidField {
            field: "'ipn' scheme-specific part",
            ..
        }
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 84 00 00 00 00")),
        Error::InvalidField {
            field: "'ipn' scheme-specific part",
            ..
        }
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 82 1B 000EE868 00000001 1B 0000000800000001")),
        Error::IpnInvalidServiceNumber(_)
    ));
}

fn expect_error(data: &[u8]) -> Error {
    hardy_cbor::decode::parse::<Eid>(data).expect_err("Parsed successfully!")
}

fn null_check(data: &[u8]) {
    assert_eq!(
        hardy_cbor::decode::parse::<Eid>(data).expect("Failed to parse"),
        Eid::Null
    );
}

fn dtn_check(data: &[u8], expected_node_name: &str, expected_demux: &str) {
    match hardy_cbor::decode::parse(data).expect("Failed to parse") {
        Eid::Dtn { node_name, demux } => {
            assert_eq!(node_name.as_ref(), expected_node_name);
            assert_eq!(demux.as_ref(), expected_demux);
        }
        _ => panic!("Not a dtn EID!"),
    };
}

fn ipn_check_legacy(
    data: &[u8],
    expected_allocator_id: u32,
    expected_node_number: u32,
    expected_service_number: u32,
) {
    match hardy_cbor::decode::parse(data).expect("Failed to parse") {
        Eid::LegacyIpn {
            allocator_id,
            node_number,
            service_number,
        } => {
            assert_eq!(expected_allocator_id, allocator_id);
            assert_eq!(expected_node_number, node_number);
            assert_eq!(expected_service_number, service_number);
        }
        _ => panic!("Not a legacy format ipn EID!"),
    };
}

fn ipn_check(
    data: &[u8],
    expected_allocator_id: u32,
    expected_node_number: u32,
    expected_service_number: u32,
) {
    match hardy_cbor::decode::parse(data).expect("Failed to parse") {
        Eid::Ipn {
            allocator_id,
            node_number,
            service_number,
        } => {
            assert_eq!(expected_allocator_id, allocator_id);
            assert_eq!(expected_node_number, node_number);
            assert_eq!(expected_service_number, service_number);
        }
        _ => panic!("Not an ipn EID!"),
    };
}
