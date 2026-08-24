use hardy_bpv7::eid::{DtnNodeId, Eid, Error, IpnNodeId};
use hex_literal::hex;

// Parse a string EID, re-encode it as CBOR, decode it again, and check
// the displayed form of the result.
fn roundtrip(eid_str: &str, expected: &str) {
    let eid = eid_str.parse::<Eid>().expect("Invalid EID");
    let cbor = hardy_cbor::encode::emit(&eid).0;
    let eid2 = hardy_cbor::decode::parse::<Eid>(&cbor).expect("Invalid CBOR");
    assert_eq!(eid2.to_string(), expected, "{eid_str} round-tripped badly");
}

#[test]
fn str_cbor_display_roundtrip() {
    for s in [
        "dtn:none",
        "dtn://node/",
        "dtn://node/service",
        "ipn:1.1",
        "ipn:1.1.1",
        "ipn:!.1",
    ] {
        roundtrip(s, s);
    }
}

// Forms that are valid on input but display as their canonical equivalent.
#[test]
fn normalising_roundtrip() {
    for (s, expected) in [
        ("ipn:0.0", "dtn:none"),
        ("ipn:0.0.0", "dtn:none"),
        ("ipn:0.1.1", "ipn:1.1"),
        ("ipn:4294967295.1", "ipn:!.1"),
    ] {
        roundtrip(s, expected);
    }
}

// An EID with an unrecognised scheme stashes the raw SSP bytes so a
// forwarding node can relay it untouched. Re-emitting must reproduce
// the wire form byte for byte.
//
// Byte layout: 82 = array(2), 03 = scheme 3, 43 010203 = 3-byte string SSP.
#[test]
fn unknown_scheme_cbor_roundtrip() {
    let input = hex!("82 03 43 010203");
    let (eid, shortest) = hardy_cbor::decode::parse::<(Eid, bool)>(&input).expect("should parse");
    let Eid::Unknown { scheme: 3, data } = &eid else {
        panic!("expected Eid::Unknown with scheme 3, got {eid:?}");
    };
    assert_eq!(
        data.as_ref(),
        hex!("43 010203").as_slice(),
        "stashed SSP bytes"
    );
    assert!(shortest, "canonical input should flag shortest=true");
    assert_eq!(
        hardy_cbor::encode::emit(&eid).0,
        input,
        "re-emit must reproduce the wire form exactly"
    );
    assert!(eid.to_string().starts_with("unknown(3):"));
}

// Display must not panic when the stashed data is truncated or garbage;
// it falls back to an error description.
#[test]
fn unknown_scheme_display_handles_garbage() {
    // 43 announces a 3-byte string but only 1 byte follows.
    let eid = Eid::Unknown {
        scheme: 3,
        data: hex!("43 01").as_slice().into(),
    };
    assert!(eid.to_string().starts_with("unknown(3):"));
}

fn ipn(allocator_id: u32, node_number: u32, service_number: u32) -> Eid {
    Eid::Ipn {
        fqnn: IpnNodeId {
            allocator_id,
            node_number,
        },
        service_number,
    }
}

fn legacy_ipn(allocator_id: u32, node_number: u32, service_number: u32) -> Eid {
    Eid::LegacyIpn {
        fqnn: IpnNodeId {
            allocator_id,
            node_number,
        },
        service_number,
    }
}

fn dtn(node_name: &str, service_name: &str) -> Eid {
    Eid::Dtn {
        node_name: DtnNodeId {
            node_name: node_name.into(),
        },
        service_name: service_name.into(),
    }
}

fn parse_str(s: &str) -> Eid {
    s.parse()
        .unwrap_or_else(|e| panic!("failed to parse {s:?}: {e}"))
}

fn expect_str_error(s: &str) {
    s.parse::<Eid>()
        .expect_err(&format!("{s:?} parsed successfully"));
}

#[test]
fn str_ipn() {
    for (s, expected) in [
        ("ipn:1.2", ipn(0, 1, 2)),
        ("ipn:1.0", ipn(0, 1, 0)),
        ("ipn:0.1.2", ipn(0, 1, 2)),
        ("ipn:0.1.0", ipn(0, 1, 0)),
        ("ipn:977000.1.3", ipn(977000, 1, 3)),
        ("ipn:977000.1.0", ipn(977000, 1, 0)),
    ] {
        assert_eq!(parse_str(s), expected, "{s}");
    }
}

#[test]
fn str_local_node() {
    for (s, service_number) in [("ipn:!.7", 7), ("ipn:!.0", 0), ("ipn:4294967295.1", 1)] {
        assert_eq!(parse_str(s), Eid::LocalNode(service_number), "{s}");
    }
}

#[test]
fn str_null() {
    for s in ["ipn:0.0", "ipn:0.0.0", "dtn:none"] {
        assert_eq!(parse_str(s), Eid::Null, "{s}");
    }
}

#[test]
fn str_dtn() {
    for (s, node_name, service_name) in [
        ("dtn://somewhere/", "somewhere", ""),
        ("dtn://somewhere/else", "somewhere", "else"),
        ("dtn://somewhere/else/", "somewhere", "else/"),
        (
            "dtn://somewhere/over/the/rainbow",
            "somewhere",
            "over/the/rainbow",
        ),
        ("dtn://somewhere//", "somewhere", "/"),
        ("dtn://somewhere//else", "somewhere", "/else"),
        ("dtn:///else", "", "else"),
    ] {
        assert_eq!(parse_str(s), dtn(node_name, service_name), "{s}");
    }
}

// Percent-encoding is decoded in the node name but preserved verbatim in
// the service name.
#[test]
fn str_dtn_percent_encoding() {
    for (s, node_name, service_name) in [
        ("dtn://somewhere%2Felse/", "somewhere/else", ""),
        (
            "dtn://somewhere/over%2Fthe/rainbow",
            "somewhere",
            "over%2Fthe/rainbow",
        ),
        (
            "dtn://somewhere%2Fover/the%2Frainbow",
            "somewhere/over",
            "the%2Frainbow",
        ),
        // From Stephan Havermans testing
        (
            "dtn://%21F0Lcomz8sXNHfnRoH2NjB62Utnq0inKdcqHpeFjHp46YOS5Qs9sbI//////{\"source\":\"ipn:1.0\",\"ti{\"source\":\"ipn:1.0\",\"timestamp\":{\"creation_time\":80790",
            "!F0Lcomz8sXNHfnRoH2NjB62Utnq0inKdcqHpeFjHp46YOS5Qs9sbI",
            "/////{\"source\":\"ipn:1.0\",\"ti{\"source\":\"ipn:1.0\",\"timestamp\":{\"creation_time\":80790",
        ),
    ] {
        assert_eq!(parse_str(s), dtn(node_name, service_name), "{s}");
    }
}

#[test]
fn str_malformed_rejected() {
    for s in [
        "",
        "dtn",
        "ipn",
        ":",
        "spaniel:",
        "dtn:",
        "dtn:/",
        "dtn:somewhere",
        "dtn:/somewhere",
        "dtn://",
        "dtn://somewhere",
        "ipn:",
        "ipn:1",
        "ipn:1.2.3.4",
    ] {
        expect_str_error(s);
    }
}

// From Stephan Havermans testing: a zero node number with a non-zero
// service number is not a valid ipn EID.
#[test]
fn str_ipn_zero_node_rejected() {
    expect_str_error("ipn:0.1");
    expect_str_error("ipn:0.0.1");
}

#[test]
fn str_ipn_overflow_rejected() {
    for s in [
        "ipn:11111111111111111111111111111.222222222222222222222222222222",
        "ipn:1.222222222222222222222222222222",
        "ipn:11111111111111111111111111111.222222222222222222222222222222.33333333333333333333333333333333333",
        "ipn:1.222222222222222222222222222222.33333333333333333333333333333333333",
        "ipn:1.2.33333333333333333333333333333333333",
    ] {
        expect_str_error(s);
    }
}

fn parse_cbor(data: &[u8]) -> Eid {
    hardy_cbor::decode::parse::<Eid>(data)
        .unwrap_or_else(|e| panic!("failed to parse {data:02x?}: {e}"))
}

fn expect_cbor_error(data: &[u8]) -> Error {
    hardy_cbor::decode::parse::<Eid>(data).expect_err("parsed successfully")
}

#[test]
fn cbor_ipn() {
    assert_eq!(parse_cbor(&hex!("82 02 82 01 01")), ipn(0, 1, 1));
    assert_eq!(parse_cbor(&hex!("82 02 83 00 01 01")), ipn(0, 1, 1));
    assert_eq!(
        parse_cbor(&hex!("82 02 83 1A 000EE868 01 01")),
        ipn(977000, 1, 1)
    );
    // 2-element form with a packed 64-bit allocator/node FQNN.
    assert_eq!(
        parse_cbor(&hex!("82 02 82 1B 000EE868 00000001 01")),
        legacy_ipn(977000, 1, 1)
    );
}

#[test]
fn cbor_null() {
    for data in [
        hex!("82 02 82 00 00").as_slice(),
        hex!("82 02 83 00 00 00").as_slice(),
        // From Stephan Havermans testing: zero node number decodes to null
        hex!("82 02 82 00 01").as_slice(),
        hex!("82 02 83 00 00 01").as_slice(),
        // Legacy dtn Text("none") form
        hex!("82 01 64 6e6f6e65").as_slice(),
    ] {
        assert_eq!(parse_cbor(data), Eid::Null, "{data:02x?}");
    }
}

#[test]
fn cbor_dtn() {
    assert_eq!(
        parse_cbor(&hex!("82 01 67 2f2f6e6f64652f")),
        dtn("node", "")
    );
    assert_eq!(
        parse_cbor(&hex!("82 01 6f 2f2f6c6f6e676e6f64656e616d652f")),
        dtn("longnodename", "")
    );
    assert_eq!(
        parse_cbor(&hex!(
            "82 01 76 2f2f6c6f6e676e6f64656e616d652f73657276696365"
        )),
        dtn("longnodename", "service")
    );
}

#[test]
fn cbor_truncated_rejected() {
    assert!(matches!(
        expect_cbor_error(&[]),
        Error::InvalidCBOR(hardy_cbor::decode::Error::NeedMoreData(1))
    ));
}

#[test]
fn cbor_ipn_overflow_rejected() {
    assert!(matches!(
        expect_cbor_error(&hex!(
            "82 02 83 1B 0000000800000001 1B 0000000800000001 1B 0000000800000001"
        )),
        Error::IpnInvalidAllocatorId(_)
    ));
    assert!(matches!(
        expect_cbor_error(&hex!("82 02 83 01 1B 0000000800000001 1B 0000000800000001")),
        Error::IpnInvalidNodeNumber(_)
    ));
    assert!(matches!(
        expect_cbor_error(&hex!("82 02 83 01 01 1B 0000000800000001")),
        Error::IpnInvalidServiceNumber(_)
    ));
    assert!(matches!(
        expect_cbor_error(&hex!("82 02 82 1B 000EE868 00000001 1B 0000000800000001")),
        Error::IpnInvalidServiceNumber(_)
    ));
}

#[test]
fn cbor_ipn_bad_arity_rejected() {
    // 1-element and 4-element ipn SSP arrays are structurally invalid.
    assert!(matches!(
        expect_cbor_error(&hex!("82 02 81 00")),
        Error::InvalidField {
            field: "'ipn' scheme-specific part",
            ..
        }
    ));
    assert!(matches!(
        expect_cbor_error(&hex!("82 02 84 00 00 00 00")),
        Error::InvalidField {
            field: "'ipn' scheme-specific part",
            ..
        }
    ));
}

/// RFC 9171 §4.1: the scheme uint MUST be encoded as a single byte (0x01
/// for dtn). A non-shortest encoding such as `0x18 0x01` is rejected.
#[test]
fn non_shortest_scheme_uint_rejected() {
    // [scheme=18 01 (non-shortest 1), "//node/"]
    let bytes = hex!("82 18 01 67 2f2f6e6f64652f");
    assert!(matches!(
        expect_cbor_error(&bytes),
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
    // [1, "none"]: non-canonical form
    let bytes = hex!("82 01 64 6e6f6e65");
    let (eid, shortest) = hardy_cbor::decode::parse::<(Eid, bool)>(&bytes).expect("should parse");
    assert_eq!(eid, Eid::Null);
    assert!(!shortest, "Text(\"none\") should flag shortest=false");

    // [1, 0]: canonical form per §4.2.5.1.1
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
    // [1, tag-0("none")]: tag on SSP
    let bytes = hex!("82 01 c0 64 6e6f6e65");
    assert!(matches!(
        expect_cbor_error(&bytes),
        Error::InvalidField {
            field: "'dtn' scheme-specific part",
            ..
        }
    ));
}
