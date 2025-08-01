use super::*;
use error::CaptureFieldErr;
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreationTimestamp {
    pub creation_time: Option<dtn_time::DtnTime>,
    pub sequence_number: u64,
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

impl hardy_cbor::encode::ToCbor for CreationTimestamp {
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| {
            a.emit(&self.creation_time.unwrap_or_default());
            a.emit(&self.sequence_number);
        })
    }
}

impl hardy_cbor::decode::FromCbor for CreationTimestamp {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse_array(data, |a, shortest, tags| {
            let (timestamp, s1) = a.parse().map_field_err("bundle creation time")?;

            let (sequence_number, s2) = a.parse().map_field_err("sequence number")?;

            Ok((
                CreationTimestamp {
                    creation_time: if timestamp == 0 {
                        None
                    } else {
                        Some(dtn_time::DtnTime::new(timestamp))
                    },
                    sequence_number,
                },
                shortest && tags.is_empty() && a.is_definite() && s1 && s2,
            ))
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}
