use super::*;
use bundle::CaptureFieldErr;

#[derive(Debug, Copy, Clone)]
pub struct HopInfo {
    pub limit: u64,
    pub count: u64,
}

impl cbor::encode::ToCbor for &HopInfo {
    fn to_cbor(self, encoder: &mut cbor::encode::Encoder) -> usize {
        encoder.emit_array(Some(2), |a, _| {
            a.emit(self.limit);
            a.emit(self.count);
        })
    }
}

impl cbor::decode::FromCbor for HopInfo {
    type Error = BundleError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |a, shortest, tags| {
            let (limit, s1, _) = a.parse::<(u64, bool, usize)>().map_field_err("hop limit")?;
            let (count, s2, _) = a.parse::<(u64, bool, usize)>().map_field_err("hop count")?;

            Ok::<_, BundleError>((
                HopInfo { limit, count },
                shortest && tags.is_empty() && a.is_definite() && s1 && s2,
            ))
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}
