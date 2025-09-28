use super::*;
use error::CaptureFieldErr;

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "bincode", derive(bincode::Encode, bincode::Decode))]
pub struct HopInfo {
    pub limit: u64,
    pub count: u64,
}

impl hardy_cbor::encode::ToCbor for HopInfo {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&(&self.limit, &self.count))
    }
}

impl hardy_cbor::decode::FromCbor for HopInfo {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, shortest, tags| {
            let (limit, s1) = a.parse().map_field_err("hop limit")?;
            let (count, s2) = a.parse().map_field_err("hop count")?;

            Ok::<_, Error>((
                HopInfo { limit, count },
                shortest && tags.is_empty() && a.is_definite() && s1 && s2,
            ))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}
