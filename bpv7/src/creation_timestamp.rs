use super::*;
use bundle::CaptureFieldErr;

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq)]
pub struct CreationTimestamp {
    pub creation_time: Option<DtnTime>,
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

impl cbor::encode::ToCbor for &CreationTimestamp {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) {
        encoder.emit_array(Some(2), |a| {
            if let Some(timestamp) = self.creation_time {
                a.emit(timestamp);
            } else {
                a.emit(0);
            }
            a.emit(self.sequence_number);
        })
    }
}

impl cbor::decode::FromCbor for CreationTimestamp {
    type Error = BundleError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |a, shortest, tags| {
            let (timestamp, s1) = a.parse().map_field_err("bundle creation time")?;

            let (sequence_number, s2) = a.parse().map_field_err("sequence number")?;

            Ok((
                CreationTimestamp {
                    creation_time: if timestamp == 0 {
                        None
                    } else {
                        Some(DtnTime::new(timestamp))
                    },
                    sequence_number,
                },
                shortest && tags.is_empty() && a.is_definite() && s1 && s2,
            ))
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}
