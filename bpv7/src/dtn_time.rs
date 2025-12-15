/*!
This module provides a representation of DTN time, which is defined as the
number of milliseconds since the DTN epoch (2000-01-01 00:00:00 UTC).
*/

const DTN_EPOCH: time::OffsetDateTime = time::macros::datetime!(2000-01-01 00:00:00 UTC);

/// Represents a DTN timestamp.
///
/// DTN time is the number of milliseconds since the DTN epoch (2000-01-01 00:00:00 UTC).
#[derive(Debug, Default, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct DtnTime(u64);

impl DtnTime {
    /// Creates a new `DtnTime` instance representing the current time.
    #[cfg(feature = "std")]
    pub fn now() -> Self {
        Self(((time::OffsetDateTime::now_utc() - DTN_EPOCH).whole_milliseconds()) as u64)
    }

    /// Creates a new `DtnTime` instance from the given number of milliseconds since the DTN epoch.
    pub fn new(millisecs: u64) -> Self {
        Self(millisecs)
    }

    /// Returns the number of milliseconds since the DTN epoch.
    pub fn millisecs(&self) -> u64 {
        self.0
    }

    pub fn saturating_from(t: time::OffsetDateTime) -> Self {
        let millisecs = (t - DTN_EPOCH).whole_milliseconds();
        if millisecs < 0 {
            Self::new(0)
        } else if millisecs > u64::MAX as i128 {
            Self::new(u64::MAX)
        } else {
            Self(millisecs as u64)
        }
    }
}

impl hardy_cbor::encode::ToCbor for DtnTime {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&self.0)
    }
}

impl hardy_cbor::decode::FromCbor for DtnTime {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse(data)
            .map(|(millisecs, shortest, len)| (Self(millisecs), shortest, len))
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
        write!(
            f,
            "{}",
            DTN_EPOCH.saturating_add(time::Duration::new(
                (self.0 / 1000) as i64,
                (self.0 % 1000 * 1_000_000) as i32,
            ))
        )
    }
}
