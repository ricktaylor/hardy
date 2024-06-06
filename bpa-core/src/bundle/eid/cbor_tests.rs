use super::*;
use hex_literal::hex;

#[test]
fn tests() {
    // Positive tests
    ipn_check2(&hex!("82 02 82 01 01"), 0, 1, 1);
    ipn_check3(&hex!("82 02 83 00 01 01"), 0, 1, 1);

    ipn_check2(&hex!("82 02 82 1B 000EE868 00000001 01"), 977000, 1, 1);
    ipn_check3(&hex!("82 02 83 1A 000EE868 01 01"), 977000, 1, 1);

    null_check(&hex!("82 02 82 00 00"));
    null_check(&hex!("82 02 83 00 00 00"));

    // TODO: Add dtn tests

    // Negative tests
    assert!(matches!(
        expect_error(&[]),
        EidError::ArrayExpected(cbor::decode::Error::NotEnoughData)
    ));
    assert!(matches!(
        expect_error(&hex!(
            "82 02 83 1B 0000000800000001 1B 0000000800000001 1B 0000000800000001"
        )),
        EidError::IpnInvalidAllocatorId(_)
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 83 01 1B 0000000800000001 1B 0000000800000001")),
        EidError::IpnInvalidNodeNumber(_)
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 83 01 01 1B 0000000800000001")),
        EidError::IpnInvalidServiceNumber(_)
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 81 00")),
        EidError::IpnInvalidComponents
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 84 00 00 00 00")),
        EidError::IpnInvalidComponents
    ));
    assert!(matches!(
        expect_error(&hex!("82 02 82 1B 000EE868 00000001 1B 0000000800000001")),
        EidError::IpnInvalidServiceNumber(_)
    ));
}

fn expect_error(data: &[u8]) -> EidError {
    cbor::decode::parse::<Eid>(data).expect_err("Parsed successfully!")
}

fn null_check(data: &[u8]) {
    assert!(matches!(
        cbor::decode::parse::<Eid>(data).expect("Failed to parse"),
        Eid::Null
    ));
}

fn ipn_check2(
    data: &[u8],
    expected_allocator_id: u32,
    expected_node_number: u32,
    expected_service_number: u32,
) {
    match cbor::decode::parse::<Eid>(data).expect("Failed to parse") {
        Eid::Ipn2 {
            allocator_id,
            node_number,
            service_number,
        } => {
            assert_eq!(expected_allocator_id, allocator_id);
            assert_eq!(expected_node_number, node_number);
            assert_eq!(expected_service_number, service_number);
        }
        _ => panic!("Not an ipn 2 EID!"),
    };
}

fn ipn_check3(
    data: &[u8],
    expected_allocator_id: u32,
    expected_node_number: u32,
    expected_service_number: u32,
) {
    match cbor::decode::parse::<Eid>(data).expect("Failed to parse") {
        Eid::Ipn3 {
            allocator_id,
            node_number,
            service_number,
        } => {
            assert_eq!(expected_allocator_id, allocator_id);
            assert_eq!(expected_node_number, node_number);
            assert_eq!(expected_service_number, service_number);
        }
        _ => panic!("Not an ipn 3 EID!"),
    };
}
