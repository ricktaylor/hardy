use super::*;
use error::CaptureFieldErr;

#[cfg(not(feature = "std"))]
static GLOBAL_COUNTER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct CreationTimestamp {
    creation_time: Option<dtn_time::DtnTime>,
    sequence_number: u64,
}

impl CreationTimestamp {
    #[cfg(feature = "std")]
    pub fn now() -> Self {
        let timestamp = time::OffsetDateTime::now_utc();
        Self {
            creation_time: Some(timestamp.try_into().unwrap()),
            sequence_number: (timestamp.nanosecond() % 1_000_000) as u64,
        }
    }

    #[cfg(not(feature = "std"))]
    pub fn new() -> Self {
        Self {
            creation_time: None,
            sequence_number: GLOBAL_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// Was the CreationTimestamp created by a source with an 'accurate clock'
    pub fn is_clocked(&self) -> bool {
        self.creation_time.is_some()
    }

    pub fn as_datetime(&self) -> Option<time::OffsetDateTime> {
        let t: time::OffsetDateTime = self.creation_time?.into();
        Some(t.saturating_add(time::Duration::nanoseconds(self.sequence_number as i64)))
    }
}

impl hardy_cbor::encode::ToCbor for CreationTimestamp {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&(
            &self.creation_time.unwrap_or_default(),
            &self.sequence_number,
        ));
    }
}

impl hardy_cbor::decode::FromCbor for CreationTimestamp {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, shortest, tags| {
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
        .map(|((v, s), len)| (v, s, len))
    }
}
