use super::*;
use base64::prelude::*;
use thiserror::Error;

#[derive(Default, Debug, Clone, Hash, PartialEq, Eq)]
pub struct BundleId {
    pub source: Eid,
    pub timestamp: CreationTimestamp,
    pub fragment_info: Option<FragmentInfo>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FragmentInfo {
    pub offset: u64,
    pub total_len: u64,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Bad bundle id key")]
    BadKey,

    #[error("Bad base64 encoding")]
    BadBase64(#[from] base64::DecodeError),

    #[error("Failed to decode {field}: {source}")]
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

impl BundleId {
    pub fn from_key(k: &str) -> Result<Self, Error> {
        cbor::decode::parse_array(&BASE64_STANDARD_NO_PAD.decode(k)?, |array, _, _| {
            let s = Self {
                source: array.parse().map_field_err("source EID")?,
                timestamp: array.parse().map_field_err("creation timestamp")?,
                fragment_info: if let Some(4) = array.count() {
                    Some(FragmentInfo {
                        offset: array.parse().map_field_err("fragment offset")?,
                        total_len: array
                            .parse()
                            .map_field_err("total application data unit Length")?,
                    })
                } else {
                    None
                },
            };
            if array.end()?.is_none() {
                Err(Error::BadKey)
            } else {
                Ok(s)
            }
        })
        .map(|v| v.0)
    }
    pub fn to_key(&self) -> String {
        BASE64_STANDARD_NO_PAD.encode(if let Some(fragment_info) = &self.fragment_info {
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
