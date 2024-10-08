use super::*;
use thiserror::Error;

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq)]
pub struct CreationTimestamp {
    pub creation_time: Option<DtnTime>,
    pub sequence_number: u64,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),
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

impl cbor::encode::ToCbor for &CreationTimestamp {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| {
            a.emit(self.creation_time);
            a.emit(self.sequence_number);
        })
    }
}

impl cbor::decode::FromCbor for CreationTimestamp {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |a, tags| {
            if !tags.is_empty() {
                return Err(cbor::decode::Error::IncorrectType(
                    "Untagged Array".to_string(),
                    "Tagged Array".to_string(),
                )
                .into());
            }

            let timestamp = a.parse().map_field_err("bundle creation time")?;
            let creation_time = CreationTimestamp {
                creation_time: if timestamp == 0 {
                    None
                } else {
                    Some(DtnTime::new(timestamp))
                },
                sequence_number: a.parse().map_field_err("sequence number")?,
            };
            Ok(creation_time)
        })
    }
}
