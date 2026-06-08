/*!
This module provides a representation of DTN time, which is defined as the
number of milliseconds since the DTN epoch (2000-01-01 00:00:00 UTC).
*/

use super::*;

const DTN_EPOCH: time::OffsetDateTime = time::macros::datetime!(2000-01-01 00:00:00 UTC);

/// Represents a DTN timestamp.
///
/// DTN time is the number of milliseconds since the DTN epoch (2000-01-01 00:00:00 UTC).
#[derive(Debug, Default, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DtnTime(u64);

impl DtnTime {
    /// Creates a new `DtnTime` instance representing the current time.
    #[cfg(feature = "std")]
    pub fn now() -> Self {
        Self::saturating_from(time::OffsetDateTime::now_utc())
    }

    /// Creates a new `DtnTime` instance from the given number of milliseconds since the DTN epoch.
    #[inline]
    pub fn new(millisecs: u64) -> Self {
        Self(millisecs)
    }

    /// Returns the number of milliseconds since the DTN epoch.
    #[inline]
    pub fn millisecs(&self) -> u64 {
        self.0
    }

    pub fn saturating_from(t: time::OffsetDateTime) -> Self {
        let millisecs = (t - DTN_EPOCH).whole_milliseconds();
        let millisecs = if millisecs < 0 {
            0
        } else if millisecs > u64::MAX as i128 {
            u64::MAX
        } else {
            millisecs as u64
        };
        Self(millisecs)
    }
}

impl hardy_cbor::encode::ToCbor for DtnTime {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&self.0)
    }
}

impl hardy_cbor::decode::FromCbor for DtnTime {
    type Error = Error;

    /// Strict-canonical decode per RFC 9171 §4.1: DTN time is a bare unsigned
    /// integer (milliseconds since the DTN epoch). Bare uints have no
    /// indefinite-length form, so the §4.1 carveout does not apply — any
    /// non-shortest encoding is rejected with `NotCanonical`, as are unexpected
    /// tags. Returns `shortest = true` on success.
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (millisecs, len) = crate::error::parse_canonical::<u64, _>(data, Error::NotCanonical)?;
        Ok((Self(millisecs), true, len))
    }
}

/// Converts a `time::OffsetDateTime` to a `DtnTime`.
///
/// This conversion can fail if the given `OffsetDateTime` is before the DTN epoch
/// or if the number of milliseconds is too large to fit in a `u64`.
impl TryFrom<time::OffsetDateTime> for DtnTime {
    type Error = time::error::ConversionRange;

    fn try_from(instant: time::OffsetDateTime) -> Result<Self, Self::Error> {
        let millisecs = (instant - DTN_EPOCH).whole_milliseconds();
        if millisecs < 0 || millisecs > u64::MAX as i128 {
            Err(time::error::ConversionRange)
        } else {
            Ok(Self(millisecs as u64))
        }
    }
}

/// Converts a `DtnTime` to a `time::OffsetDateTime`.
impl From<DtnTime> for time::OffsetDateTime {
    fn from(dtn_time: DtnTime) -> Self {
        DTN_EPOCH.saturating_add(time::Duration::new(
            (dtn_time.0 / 1000) as i64,
            (dtn_time.0 % 1000 * 1_000_000) as i32,
        ))
    }
}

impl core::fmt::Display for DtnTime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", time::OffsetDateTime::from(*self))
    }
}
