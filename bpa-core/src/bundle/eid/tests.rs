use super::*;

#[test]
fn tests() {
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
    dtn_check(
        "dtn://somewhere/over/the/rainbow",
        "somewhere",
        "over/the/rainbow",
    );
}

fn null_check(s: &str) {
    let Eid::Null = s.parse::<Eid>().expect("Failed to parse") else {
        panic!("Not a Null EID!")
    };
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
    assert_eq!(node_name, expected_node_name);
    assert_eq!(demux.join("/"), expected_demux);
}
