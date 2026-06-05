/*!
This module defines the `BundleAge` newtype, the encoded body of the
RFC 9171 §4.4.2 Bundle Age extension block. The block carries the
number of milliseconds elapsed between the bundle's creation and the
current point in its lifetime, used by intermediate nodes when the
source had no usable clock at creation.

The on-wire encoding is a bare unsigned integer (no array, no tag).
Bare uints have **no** indefinite-length form, so the §4.1 carveout
that `HopInfo` exploits does not apply here — any non-shortest
encoding is an unambiguous canonical-CBOR violation and is rejected.
*/

use super::*;

/// Bundle age in milliseconds (RFC 9171 §4.4.2).
///
/// Constructed via `From<u64>` or `From<core::time::Duration>` (saturating
/// at `u64::MAX` milliseconds). Convert back with `u64::from(age)` or
/// `core::time::Duration::from(age)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BundleAge(u64);

impl From<u64> for BundleAge {
    fn from(millis: u64) -> Self {
        Self(millis)
    }
}

impl From<BundleAge> for u64 {
    fn from(age: BundleAge) -> Self {
        age.0
    }
}

impl From<BundleAge> for core::time::Duration {
    fn from(age: BundleAge) -> Self {
        core::time::Duration::from_millis(age.0)
    }
}

impl From<core::time::Duration> for BundleAge {
    fn from(d: core::time::Duration) -> Self {
        Self(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

impl hardy_cbor::encode::ToCbor for BundleAge {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&self.0)
    }
}

impl hardy_cbor::decode::FromCbor for BundleAge {
    type Error = Error;

    /// Strict-canonical decode per RFC 9171 §4.1 + §4.4.2:
    ///   * Body is a bare unsigned integer. Bare uints have no
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
mod tests {
    use super::*;
    use hardy_cbor::decode::FromCbor;
    use hex_literal::hex;

    /// Canonical encoding of `BundleAge(0)` is the single byte `0x00`.
    #[test]
    fn accepts_zero() {
        let body = hex!("00");
        let (v, s, len) = BundleAge::from_cbor(&body).unwrap();
        assert_eq!(v, BundleAge(0));
        assert!(s);
        assert_eq!(len, 1);
    }

    /// `BundleAge(1_000_000)` — a typical millisecond figure, two-byte
    /// canonical encoding.
    #[test]
    fn accepts_typical_value() {
        let body = hex!("1A 000F4240"); // uint 1_000_000
        let (v, s, len) = BundleAge::from_cbor(&body).unwrap();
        assert_eq!(v, BundleAge(1_000_000));
        assert!(s);
        assert_eq!(len, 5);
    }

    /// Non-shortest encoding of `0` (using the 1-byte argument form
    /// `0x18 0x00` instead of the canonical `0x00`) is rejected.
    /// Bare uints have no §4.1 carveout — this is a real violation.
    #[test]
    fn rejects_non_shortest_zero() {
        let body = hex!("18 00"); // uint 0, 1-byte argument
        assert!(matches!(
            BundleAge::from_cbor(&body),
            Err(Error::NotCanonical)
        ));
    }

    /// Non-shortest encoding of `1000` using the 4-byte argument form
    /// instead of the canonical 2-byte form.
    #[test]
    fn rejects_non_shortest_uint() {
        let body = hex!("1A 000003E8"); // uint 1000 as 4 bytes (canonical is `19 03E8`)
        assert!(matches!(
            BundleAge::from_cbor(&body),
            Err(Error::NotCanonical)
        ));
    }

    /// Tagged encoding is rejected (RFC 9171 §4.1 disallows unexpected
    /// tags on canonical bodies).
    #[test]
    fn rejects_tagged() {
        let body = hex!("C0 00"); // tag(0) on a uint
        assert!(matches!(
            BundleAge::from_cbor(&body),
            Err(Error::NotCanonical)
        ));
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
            let encoded = hardy_cbor::encode::emit(&BundleAge(millis)).0;
            let (decoded, s, len) = BundleAge::from_cbor(&encoded).unwrap();
            assert_eq!(decoded, BundleAge(millis));
            assert!(s);
            assert_eq!(len, encoded.len());
        }
    }

    /// `Duration` conversion is saturating at `u64::MAX` ms (the upper
    /// bound of what the wire format can carry).
    #[test]
    fn duration_round_trip_saturates() {
        let huge = core::time::Duration::from_secs(u64::MAX);
        assert_eq!(BundleAge::from(huge), BundleAge(u64::MAX));
    }
}
