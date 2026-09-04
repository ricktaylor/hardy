use hardy_bpv7::{Error, hop_info::HopInfo};
use hardy_cbor::decode::FromCbor;
use hex_literal::hex;

/// RFC 9171 §4.4.3: "The hop limit MUST be in the range 1 through 255."
/// Reported via the issue tracker — a real-world bundle from a
/// third-party conformance tool encoding `HopCount` with `limit=0`.
/// The HopCount block body is `82 00 00` — array of two zeros.
#[test]
fn rejects_hop_limit_zero_body() {
    // Direct HopInfo body: array [limit=0, count=0]
    let body = hex!("820000");
    assert!(matches!(
        HopInfo::from_cbor(&body),
        Err(Error::InvalidHopLimit(0))
    ));
}

/// `limit = 256` is one above the §4.4.3 range.
#[test]
fn rejects_hop_limit_256() {
    // [256, 0] — uint 256 encoded as `0x19 0x01 0x00`
    let body = hex!("82 19 0100 00");
    assert!(matches!(
        HopInfo::from_cbor(&body),
        Err(Error::InvalidHopLimit(256))
    ));
}

/// Boundary: `limit = 1` is the lowest legal value.
#[test]
fn accepts_hop_limit_1() {
    let body = hex!("820100");
    let (v, _, _) = HopInfo::from_cbor(&body).unwrap();
    assert_eq!(v.limit, 1);
    assert_eq!(v.count, 0);
}

/// Boundary: `limit = 255` is the highest legal value.
#[test]
fn accepts_hop_limit_255() {
    // [255, 0] — uint 255 encoded as `0x18 0xFF`
    let body = hex!("82 18 ff 00");
    let (v, _, _) = HopInfo::from_cbor(&body).unwrap();
    assert_eq!(v.limit, 255);
    assert_eq!(v.count, 0);
}
