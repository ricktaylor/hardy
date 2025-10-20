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
