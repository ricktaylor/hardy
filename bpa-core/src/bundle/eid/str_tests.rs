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
    dtn_check("dtn://somewhere%2Felse/", "somewhere%2Felse", "");
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
        "somewhere%2Fover",
        "the%2Frainbow",
    );

    // Negative tests
    assert!(matches!(expect_error(""), EidError::MissingScheme));
    assert!(matches!(expect_error("dtn"), EidError::MissingScheme));
    assert!(matches!(expect_error("ipn"), EidError::MissingScheme));
    assert!(matches!(expect_error(":"), EidError::UnsupportedScheme(s) if s == ""));
    assert!(matches!(expect_error("spaniel:"), EidError::UnsupportedScheme(s) if s == "spaniel"));

    assert!(matches!(expect_error("dtn:"), EidError::DtnMissingPrefix));
    assert!(matches!(expect_error("dtn:/"), EidError::DtnMissingPrefix));
    assert!(matches!(
        expect_error("dtn:somewhere"),
        EidError::DtnMissingPrefix
    ));
    assert!(matches!(
        expect_error("dtn:/somewhere"),
        EidError::DtnMissingPrefix
    ));
    assert!(matches!(expect_error("dtn://"), EidError::DtnMissingSlash));
    assert!(matches!(
        expect_error("dtn://somewhere"),
        EidError::DtnMissingSlash
    ));
    assert!(matches!(
        expect_error("dtn:///else"),
        EidError::DtnNodeNameEmpty
    ));
    assert!(matches!(
        expect_error("dtn://somewhere//"),
        EidError::DtnEmptyDemuxPart
    ));
    assert!(matches!(
        expect_error("dtn://somewhere//else"),
        EidError::DtnEmptyDemuxPart
    ));

    assert!(matches!(
        expect_error("ipn:"),
        EidError::IpnInvalidComponents
    ));
    assert!(matches!(
        expect_error("ipn:1"),
        EidError::IpnInvalidComponents
    ));
    assert!(matches!(
        expect_error("ipn:1.2.3.4"),
        EidError::IpnInvalidComponents
    ));

    assert!(
        matches!(expect_error("ipn:11111111111111111111111111111.222222222222222222222222222222"), EidError::InvalidField{ field, ..} if field == "Node Number")
    );
    assert!(
        matches!(expect_error("ipn:1.222222222222222222222222222222"), EidError::InvalidField{ field, ..} if field == "Service Number")
    );
    assert!(
        matches!(expect_error("ipn:11111111111111111111111111111.222222222222222222222222222222.33333333333333333333333333333333333"), EidError::InvalidField{ field, ..} if field == "Allocator Identifier")
    );
    assert!(
        matches!(expect_error("ipn:1.222222222222222222222222222222.33333333333333333333333333333333333"), EidError::InvalidField{ field, ..} if field == "Node Number")
    );
    assert!(
        matches!(expect_error("ipn:1.2.33333333333333333333333333333333333"), EidError::InvalidField{ field, ..} if field == "Service Number")
    );
}

fn expect_error(s: &str) -> EidError {
    s.parse::<Eid>().expect_err("Parsed successfully!")
}

fn null_check(s: &str) {
    assert!(matches!(
        s.parse::<Eid>().expect("Failed to parse"),
        Eid::Null
    ));
}

fn local_node_check(s: &str, expected_service_number: u32) {
    let Eid::LocalNode { service_number } = s.parse::<Eid>().expect("Failed to parse") else {
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
    match s.parse::<Eid>().expect("Failed to parse") {
        Eid::Ipn2 {
            allocator_id,
            node_number,
            service_number,
        }
        | Eid::Ipn3 {
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

fn dtn_check(s: &str, expected_node_name: &str, expected_demux: &str) {
    let Eid::Dtn { node_name, demux } = s.parse::<Eid>().expect("Failed to parse") else {
        panic!("Not a dtn EID!")
    };
    assert_eq!(urlencoding::encode(&node_name), expected_node_name);
    assert_eq!(
        demux
            .iter()
            .map(|s| urlencoding::encode(s))
            .collect::<Vec<std::borrow::Cow<str>>>()
            .join("/"),
        expected_demux
    );
}
