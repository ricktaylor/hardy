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

fn expect_error(s: &str) -> error::Error {
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
    let Eid::LocalNode { service_number } = s.parse().expect("Failed to parse") else {
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
            allocator_id,
            node_number,
            service_number,
        }
        | Eid::Ipn {
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
    let Eid::Dtn { node_name, demux } = s.parse().expect("Failed to parse") else {
        panic!("Not a dtn EID!")
    };
    assert_eq!(node_name.as_ref(), expected_node_name);
    assert_eq!(demux.as_ref(), expected_demux);
}
