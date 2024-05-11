use super::*;
use base64::prelude::*;

#[derive(Default, Debug)]
pub struct BundleId {
    pub source: Eid,
    pub timestamp: CreationTimestamp,
    pub fragment_info: Option<FragmentInfo>,
}

#[derive(Debug, Copy, Clone)]
pub struct FragmentInfo {
    pub offset: u64,
    pub total_len: u64,
}

impl BundleId {
    pub fn from_key(k: &str) -> Result<Self, anyhow::Error> {
        cbor::decode::parse_array(&BASE64_STANDARD_NO_PAD.decode(k)?, |array, _| {
            let s = Self {
                source: array.parse()?,
                timestamp: array.parse()?,
                fragment_info: if let Some(4) = array.count() {
                    Some(FragmentInfo {
                        offset: array.parse()?,
                        total_len: array.parse()?,
                    })
                } else {
                    None
                },
            };
            array
                .end_or_else(|| anyhow!("Bad bundle id key"))
                .map(|_| s)
        })
        .map(|(v, _)| v)
    }
    pub fn to_key(&self) -> String {
        BASE64_STANDARD_NO_PAD.encode(if let Some(fragment_info) = self.fragment_info {
            cbor::encode::emit_array(Some(4), |array| {
                array.emit(&self.source);
                array.emit(&self.timestamp);
                array.emit(fragment_info.offset);
                array.emit(fragment_info.total_len);
            })
        } else {
            cbor::encode::emit_array(Some(2), |array| {
                array.emit(&self.source);
                array.emit(&self.timestamp);
            })
        })
    }
}
