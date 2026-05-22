/*!
This module defines the `Lifetime` newtype, the encoded representation
of the RFC 9171 §4.2.1 bundle lifetime field carried in the primary
block. The lifetime is the maximum number of milliseconds the bundle
may live in the network from its creation timestamp before forwarders
should drop it.

The on-wire encoding is a bare unsigned integer (no array, no tag).
Bare uints have **no** indefinite-length form, so the §4.1 carveout
that `HopInfo` exploits does not apply — any non-shortest encoding
is an unambiguous canonical-CBOR violation and is rejected.
*/

use super::*;

/// Bundle lifetime in milliseconds (RFC 9171 §4.2.1).
///
/// Constructed via `From<u64>` or `From<core::time::Duration>` (saturating
/// at `u64::MAX` milliseconds — the upper bound of the wire format).
/// Convert back with `u64::from(lifetime)` or
/// `core::time::Duration::from(lifetime)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Lifetime(pub u64);

impl From<u64> for Lifetime {
    fn from(millis: u64) -> Self {
        Self(millis)
    }
}

impl From<Lifetime> for u64 {
    fn from(lifetime: Lifetime) -> Self {
        lifetime.0
    }
}

impl From<Lifetime> for core::time::Duration {
    fn from(lifetime: Lifetime) -> Self {
        core::time::Duration::from_millis(lifetime.0)
    }
}

impl From<core::time::Duration> for Lifetime {
    fn from(d: core::time::Duration) -> Self {
        Self(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

impl hardy_cbor::encode::ToCbor for Lifetime {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&self.0)
    }
}

impl hardy_cbor::decode::FromCbor for Lifetime {
    type Error = Error;

    /// Strict-canonical decode per RFC 9171 §4.1 + §4.2.1:
    ///   * Field is a bare unsigned integer. Bare uints have no
    ///     indefinite-length form, so the §4.1 carveout does not apply
    ///     and any non-shortest encoding is rejected with `NotCanonical`.
    ///   * Unexpected tags are rejected with `NotCanonical`.
    ///   * Returns `shortest = true` on success (no encoder discretion
    ///     left to surface), so callers can drop the flag check.
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (v, shortest, len) =
            hardy_cbor::decode::parse::<(u64, bool, usize)>(data).map_err(Error::InvalidCBOR)?;
        if !shortest {
            return Err(Error::NotCanonical);
        }
        Ok((Self(v), true, len))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use hardy_cbor::decode::FromCbor;
    use hex_literal::hex;

    /// Canonical encoding of `Lifetime(0)` is the single byte `0x00`.
    #[test]
    fn accepts_zero() {
        let body = hex!("00");
        let (v, s, len) = Lifetime::from_cbor(&body).unwrap();
        assert_eq!(v, Lifetime(0));
        assert!(s);
        assert_eq!(len, 1);
    }

    /// `Lifetime(86_400_000)` — 24 hours in milliseconds, the default
    /// in `Builder::new`. Three-byte canonical encoding.
    #[test]
    fn accepts_one_day() {
        let body = hex!("1A 05265C00"); // uint 86_400_000
        let (v, s, len) = Lifetime::from_cbor(&body).unwrap();
        assert_eq!(v, Lifetime(86_400_000));
        assert!(s);
        assert_eq!(len, 5);
    }

    /// Non-shortest encoding of `0` (using the 1-byte argument form
    /// `0x18 0x00` instead of the canonical `0x00`) is rejected. Bare
    /// uints have no §4.1 carveout — this is a real violation.
    #[test]
    fn rejects_non_shortest_zero() {
        let body = hex!("18 00"); // uint 0, 1-byte argument
        assert!(matches!(
            Lifetime::from_cbor(&body),
            Err(Error::NotCanonical)
        ));
    }

    /// Non-shortest encoding of `1000` using the 4-byte argument form
    /// instead of the canonical 2-byte form.
    #[test]
    fn rejects_non_shortest_uint() {
        let body = hex!("1A 000003E8"); // uint 1000 as 4 bytes (canonical is `19 03E8`)
        assert!(matches!(
            Lifetime::from_cbor(&body),
            Err(Error::NotCanonical)
        ));
    }

    /// Tagged encoding is rejected (RFC 9171 §4.1 disallows unexpected
    /// tags on canonical bodies).
    #[test]
    fn rejects_tagged() {
        let body = hex!("C0 00"); // tag(0) on a uint
        assert!(Lifetime::from_cbor(&body).is_err());
    }

    /// Round-trip: encode a value, decode it back, verify equality and
    /// canonical-form flag.
    #[test]
    fn round_trip() {
        for &millis in &[
            0u64,
            1,
            23,
            24,
            255,
            256,
            65535,
            65536,
            u32::MAX as u64,
            u64::MAX,
        ] {
            let encoded = hardy_cbor::encode::emit(&Lifetime(millis)).0;
            let (decoded, s, len) = Lifetime::from_cbor(&encoded).unwrap();
            assert_eq!(decoded, Lifetime(millis));
            assert!(s);
            assert_eq!(len, encoded.len());
        }
    }

    /// `Duration` conversion is saturating at `u64::MAX` ms (the upper
    /// bound of what the wire format can carry).
    #[test]
    fn duration_saturates() {
        let huge = core::time::Duration::from_secs(u64::MAX);
        assert_eq!(Lifetime::from(huge), Lifetime(u64::MAX));
    }
}
