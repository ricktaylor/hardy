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

fn dtn_check(data: &[u8], expected_node_name: &str, expected_service_name: &str) {
    match hardy_cbor::decode::parse(data).expect("Failed to parse") {
        Eid::Dtn {
            node_name,
            service_name,
        } => {
            assert_eq!(node_name.node_name.as_ref(), expected_node_name);
            assert_eq!(service_name.as_ref(), expected_service_name);
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
            fqnn:
                IpnNodeId {
                    allocator_id,
                    node_number,
                },
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
            fqnn:
                IpnNodeId {
                    allocator_id,
                    node_number,
                },
            service_number,
        } => {
            assert_eq!(expected_allocator_id, allocator_id);
            assert_eq!(expected_node_number, node_number);
            assert_eq!(expected_service_number, service_number);
        }
        _ => panic!("Not an ipn EID!"),
    };
}

/// RFC 9171 §4.1: the scheme uint MUST be encoded as a single byte (0x01
/// for dtn). A non-shortest encoding such as `0x18 0x01` is rejected.
#[test]
fn non_shortest_scheme_uint_rejected() {
    // [scheme=18 01 (non-shortest 1), "//node/"]
    let bytes = hex!("82 18 01 67 2f2f6e6f64652f");
    assert!(matches!(
        expect_error(&bytes),
        Error::InvalidField {
            field: "EID scheme",
            ..
        }
    ));
}

/// RFC 9171 §4.1 carveout: indefinite-length outer EID array is permitted
/// but the returned `shortest` flag must be `false` so callers can opt
/// to re-emit in canonical form.
#[test]
fn indefinite_outer_array_accepted_but_flagged() {
    // 9f ... ff = indefinite-length array of [1, "//node/"]
    let bytes = hex!("9f 01 67 2f2f6e6f64652f ff");
    let (eid, shortest) = hardy_cbor::decode::parse::<(Eid, bool)>(&bytes).expect("should parse");
    assert!(matches!(eid, Eid::Dtn { .. }));
    assert!(
        !shortest,
        "indefinite outer array should flag shortest=false"
    );
}

/// RFC 9171 §4.2.5.1.1: dtn null MUST be encoded as `uint 0`. The legacy
/// `Text("none")` form is accepted but must flag `shortest = false` to
/// queue a rewrite. The canonical `uint 0` form must flag `shortest = true`.
#[test]
fn dtn_null_canonicality() {
    // [1, "none"] — non-canonical form
    let bytes = hex!("82 01 64 6e6f6e65");
    let (eid, shortest) = hardy_cbor::decode::parse::<(Eid, bool)>(&bytes).expect("should parse");
    assert_eq!(eid, Eid::Null);
    assert!(!shortest, "Text(\"none\") should flag shortest=false");

    // [1, 0] — canonical form per §4.2.5.1.1
    let bytes = hex!("82 01 00");
    let (eid, shortest) = hardy_cbor::decode::parse::<(Eid, bool)>(&bytes).expect("should parse");
    assert_eq!(eid, Eid::Null);
    assert!(shortest, "uint 0 form should flag shortest=true");
}

/// RFC 9171 §4.1: unexpected tags on a CBOR item are a canonicality
/// violation. A tagged dtn SSP (e.g. tag 0 wrapping the text) must be
/// rejected as `NotCanonical` rather than as a structural type error.
#[test]
fn tagged_dtn_ssp_rejected_as_not_canonical() {
    // [1, tag-0("none")] — tag on SSP
    let bytes = hex!("82 01 c0 64 6e6f6e65");
    assert!(matches!(
        expect_error(&bytes),
        Error::InvalidField {
            field: "'dtn' scheme-specific part",
            ..
        }
    ));
}
