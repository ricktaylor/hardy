use super::*;
use thiserror::Error;

#[derive(Default, Debug, Copy, Clone)]
pub struct CreationTimestamp {
    pub creation_time: Option<DtnTime>,
    pub sequence_number: u64,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Additional items found in Creation timestamp array")]
    AdditionalItems,

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("Expecting CBOR array")]
    ArrayExpected(#[from] cbor::decode::Error),
}

trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, Error>;
}

impl CreationTimestamp {
    pub fn now() -> Self {
        let timestamp = time::OffsetDateTime::now_utc();
        Self {
            creation_time: Some(timestamp.try_into().unwrap()),
            sequence_number: (timestamp.nanosecond() % 1_000_000) as u64,
        }
    }
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, Error> {
        self.map_err(|e| Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}

impl cbor::encode::ToCbor for CreationTimestamp {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| {
            a.emit(self.creation_time);
            a.emit(self.sequence_number);
        })
    }
}

impl cbor::decode::FromCbor for CreationTimestamp {
    type Error = self::Error;

    fn try_from_cbor_tagged(data: &[u8]) -> Result<Option<(Self, usize, Vec<u64>)>, Self::Error> {
        cbor::decode::try_parse_array(data, |a, tags| {
            let timestamp = a.parse().map_field_err("bundle creation time")?;
            let creation_time = CreationTimestamp {
                creation_time: if timestamp == 0 {
                    None
                } else {
                    Some(DtnTime::new(timestamp))
                },
                sequence_number: a.parse().map_field_err("sequence number")?,
            };
            if a.end()?.is_none() {
                return Err(Error::AdditionalItems);
            }
            Ok((creation_time, tags))
        })
        .map(|r| r.map(|((t, tags), len)| (t, len, tags)))
    }
}
