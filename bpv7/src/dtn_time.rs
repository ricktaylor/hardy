use super::*;

const DTN_EPOCH: time::OffsetDateTime = time::macros::datetime!(2000-01-01 00:00:00 UTC);

#[derive(Debug, Default, Copy, Clone)]
pub struct DtnTime {
    millisecs: u64,
}

impl DtnTime {
    pub fn now() -> Self {
        Self {
            millisecs: ((time::OffsetDateTime::now_utc() - DTN_EPOCH).whole_milliseconds()) as u64,
        }
    }

    pub fn new(millisecs: u64) -> Self {
        Self { millisecs }
    }

    pub fn millisecs(&self) -> u64 {
        self.millisecs
    }
}

impl cbor::encode::ToCbor for DtnTime {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit(self.millisecs)
    }
}

impl cbor::decode::FromCbor for DtnTime {
    type Error = cbor::decode::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        if let Some((millisecs, len, tags)) = u64::try_from_cbor_tagged(data)? {
            Ok(Some((Self { millisecs }, len, tags)))
        } else {
            Ok(None)
        }
    }
}

impl TryFrom<time::OffsetDateTime> for DtnTime {
    type Error = time::error::ConversionRange;

    fn try_from(instant: time::OffsetDateTime) -> Result<Self, Self::Error> {
        let millisecs = (instant - DTN_EPOCH).whole_milliseconds();
        if millisecs < 0 || millisecs > u64::MAX as i128 {
            Err(time::error::ConversionRange)
        } else {
            Ok(Self {
                millisecs: millisecs as u64,
            })
        }
    }
}

impl From<DtnTime> for time::OffsetDateTime {
    fn from(dtn_time: DtnTime) -> Self {
        DTN_EPOCH.saturating_add(time::Duration::saturating_seconds_f64(
            (dtn_time.millisecs / 1_000) as f64 + ((dtn_time.millisecs % 1_0000) as f64 / 1_000f64),
        ))
    }
}
