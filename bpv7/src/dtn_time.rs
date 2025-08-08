const DTN_EPOCH: time::OffsetDateTime = time::macros::datetime!(2000-01-01 00:00:00 UTC);

#[derive(Debug, Default, Copy, Clone, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct DtnTime(u64);

impl DtnTime {
    pub fn now() -> Self {
        Self(((time::OffsetDateTime::now_utc() - DTN_EPOCH).whole_milliseconds()) as u64)
    }

    pub fn new(millisecs: u64) -> Self {
        Self(millisecs)
    }

    pub fn millisecs(&self) -> u64 {
        self.0
    }
}

impl hardy_cbor::encode::ToCbor for DtnTime {
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(&self.0)
    }
}

impl hardy_cbor::decode::FromCbor for DtnTime {
    type Error = hardy_cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse(data)
            .map(|o| o.map(|(millisecs, shortest, len)| (Self(millisecs), shortest, len)))
    }
}

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

impl From<DtnTime> for time::OffsetDateTime {
    fn from(dtn_time: DtnTime) -> Self {
        DTN_EPOCH.saturating_add(time::Duration::new(
            (dtn_time.0 / 1000) as i64,
            (dtn_time.0 % 1000 * 1_000_000) as i32,
        ))
    }
}
