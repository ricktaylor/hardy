/*!
This module defines the `HopInfo` struct, which is used to track the hop limit
and hop count of a bundle as it traverses the network. This information is
typically part of the bundle's primary block and is used to prevent infinite
loops and to control the bundle's lifetime in the network.
*/

use super::*;
use error::require_canonical;

/// Contains hop limit and hop count information for a bundle.
///
/// The hop limit is the maximum number of hops a bundle is allowed to traverse,
/// while the hop count is the number of hops it has already traversed.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct HopInfo {
    /// The maximum number of hops the bundle is allowed to traverse.
    pub limit: u64,
    /// The number of hops the bundle has already traversed.
    pub count: u64,
}

impl hardy_cbor::encode::ToCbor for HopInfo {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&(&self.limit, &self.count))
    }
}

impl hardy_cbor::decode::FromCbor for HopInfo {
    type Error = Error;

    /// Strict-canonical decode per RFC 9171 §4.1 plus §4.4.3 range check:
    ///   * Non-shortest array head, non-shortest sub-field encoding, and
    ///     unexpected tags are rejected with `NotCanonical`.
    ///   * Indefinite-length array encoding is accepted (§4.1 carveout)
    ///     and reflected in the returned `shortest` flag as `false` so
    ///     callers can opt to re-emit in canonical form.
    ///   * `limit` MUST be in `1..=255` (§4.4.3); otherwise rejected
    ///     with `InvalidHopLimit`. The `count` field has no RFC-mandated
    ///     range (only a SHOULD that it start at 0 and increment).
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, shortest, tags| {
            if !shortest || !tags.is_empty() {
                return Err(Error::NotCanonical);
            }
            let limit = require_canonical(a, "hop limit")?;
            if limit == 0 || limit > 255 {
                return Err(Error::InvalidHopLimit(limit));
            }
            let count = require_canonical(a, "hop count")?;
            // `shortest` here means "would round-trip to identical bytes
            // under canonical emission" — i.e. the array was definite-
            // length. Indefinite arrays are RFC-permitted but trigger
            // a re-emit if the caller wants canonical bytes.
            Ok::<_, Error>((HopInfo { limit, count }, a.is_definite()))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
