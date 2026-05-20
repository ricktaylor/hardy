//! Integration tests for `hardy_bpv7::eid::Eid` — string parsing, CBOR
//! decoding, and roundtrip canonicalisation via the public API.

use hardy_bpv7::eid::{Eid, Error, IpnNodeId};

mod str_tests {
    use super::*;

    #[test]
    fn tests() {
        // Positive tests
        ipn_check("ipn:1.2", 0, 1, 2);
        ipn_check("ipn:1.0", 0, 1, 0);
        ipn_check("ipn:0.1.2", 0, 1, 2);
        ipn_check("ipn:0.1.0", 0, 1, 0);
        ipn_check("ipn:977000.1.3", 977000, 1, 3);
        ipn_check("ipn:977000.1.0", 977000, 1, 0);

        local_node_check("ipn:!.7", 7);
        local_node_check("ipn:!.0", 0);

        null_check("ipn:0.0");
        null_check("ipn:0.0.0");
        null_check("dtn:none");

        dtn_check("dtn://somewhere/", "somewhere", "");
        dtn_check("dtn://somewhere/else", "somewhere", "else");
        dtn_check("dtn://somewhere/else/", "somewhere", "else/");
        dtn_check("dtn://somewhere%2Felse/", "somewhere/else", "");
        dtn_check(
            "dtn://somewhere/over/the/rainbow",
            "somewhere",
            "over/the/rainbow",
        );
        dtn_check(
            "dtn://somewhere/over%2Fthe/rainbow",
            "somewhere",
            "over%2Fthe/rainbow",
        );
        dtn_check(
            "dtn://somewhere%2Fover/the%2Frainbow",
            "somewhere/over",
            "the%2Frainbow",
        );

        dtn_check("dtn://somewhere//", "somewhere", "/");
        dtn_check("dtn://somewhere//else", "somewhere", "/else");
        dtn_check("dtn:///else", "", "else");

        dtn_check(
            "dtn://%21F0Lcomz8sXNHfnRoH2NjB62Utnq0inKdcqHpeFjHp46YOS5Qs9sbI//////{\"source\":\"ipn:1.0\",\"ti{\"source\":\"ipn:1.0\",\"timestamp\":{\"creation_time\":80790",
            "!F0Lcomz8sXNHfnRoH2NjB62Utnq0inKdcqHpeFjHp46YOS5Qs9sbI",
            "/////{\"source\":\"ipn:1.0\",\"ti{\"source\":\"ipn:1.0\",\"timestamp\":{\"creation_time\":80790",
        );

        // Negative tests
        expect_error("");
        expect_error("dtn");
        expect_error("ipn");
        expect_error(":");
        expect_error("spaniel:");

        expect_error("dtn:");
        expect_error("dtn:/");
        expect_error("dtn:somewhere");
        expect_error("dtn:/somewhere");
        expect_error("dtn://");
        expect_error("dtn://somewhere");

        expect_error("ipn:");
        expect_error("ipn:1");
        expect_error("ipn:1.2.3.4");

        // From Stephan Havermans testing
        expect_error("ipn:0.1");
        expect_error("ipn:0.0.1");

        expect_error("ipn:11111111111111111111111111111.222222222222222222222222222222");
        expect_error("ipn:1.222222222222222222222222222222");
        expect_error(
            "ipn:11111111111111111111111111111.222222222222222222222222222222.33333333333333333333333333333333333",
        );
        expect_error("ipn:1.222222222222222222222222222222.33333333333333333333333333333333333");
        expect_error("ipn:1.2.33333333333333333333333333333333333");
    }

    fn expect_error(s: &str) -> Error {
        s.parse::<Eid>()
            .expect_err(&format!("\"{s}\" Parsed successfully!"))
    }

    fn null_check(s: &str) {
        assert!(matches!(
            s.parse::<Eid>()
                .unwrap_or_else(|_| panic!("Failed to parse \"{s}\"")),
            Eid::Null
        ));
    }

    fn local_node_check(s: &str, expected_service_number: u32) {
        let Eid::LocalNode(service_number) = s.parse().expect("Failed to parse") else {
            panic!("Not a LocalNode EID!")
        };
        assert_eq!(expected_service_number, service_number);
    }

    fn ipn_check(
        s: &str,
        expected_allocator_id: u32,
        expected_node_number: u32,
        expected_service_number: u32,
    ) {
        match s.parse().expect("Failed to parse") {
            Eid::LegacyIpn {
                fqnn:
                    IpnNodeId {
                        allocator_id,
                        node_number,
                    },
                service_number,
            }
            | Eid::Ipn {
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

    fn dtn_check(s: &str, expected_node_name: &str, expected_service_name: &str) {
        let Eid::Dtn {
            node_name,
            service_name,
        } = s.parse().expect("Failed to parse")
        else {
            panic!("Not a dtn EID!")
        };
        assert_eq!(node_name.node_name.as_ref(), expected_node_name);
        assert_eq!(service_name.as_ref(), expected_service_name);
    }
}

mod cbor_tests {
    use super::*;
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
}

mod roundtrip_tests {
    use super::*;

    #[test]
    fn tests() {
        roundtrip_eid("dtn:none");
        roundtrip_eid("dtn://node/");
        roundtrip_eid("dtn://node/service");
        roundtrip_eid("ipn:1.1");
        roundtrip_eid("ipn:1.1.1");
        roundtrip_eid("ipn:!.1");

        roundtrip_eid_almost("ipn:0.0", "dtn:none");
        roundtrip_eid_almost("ipn:0.0.0", "dtn:none");
        roundtrip_eid_almost("ipn:0.1.1", "ipn:1.1");
        roundtrip_eid_almost("ipn:4294967295.1", "ipn:!.1");
    }

    fn roundtrip_eid_almost(eid_str: &str, expected: &str) {
        let eid = eid_str.parse::<Eid>().expect("Invalid EID");
        let cbor = hardy_cbor::encode::emit(&eid).0;
        let eid2 = hardy_cbor::decode::parse::<Eid>(&cbor).expect("Invalid CBOR");
        let eid_str2 = eid2.to_string();
        assert_eq!(eid_str2, expected);
    }

    fn roundtrip_eid(eid_str: &str) {
        roundtrip_eid_almost(eid_str, eid_str);
    }
}
