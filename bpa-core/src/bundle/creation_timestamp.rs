use super::*;
use thiserror::Error;

#[derive(Default, Debug, Copy, Clone)]
pub struct CreationTimestamp {
    pub creation_time: u64,
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
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), Self::Error> {
        cbor::decode::parse_array(data, |a, tags| {
            let ct = CreationTimestamp {
                creation_time: a.parse().map_field_err("bundle creation time")?,
                sequence_number: a.parse().map_field_err("sequence number")?,
            };
            if a.end()?.is_none() {
                return Err(Error::AdditionalItems);
            }
            Ok((ct, tags.to_vec()))
        })
        .map(|((t, tags), len)| (t, len, tags))
    }
}
