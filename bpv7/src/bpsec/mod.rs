use super::*;

pub mod bcb;
pub mod bib;
pub mod key;

mod error;
mod parse;

#[cfg(feature = "rfc9173")]
mod rfc9173;

use error::CaptureFieldErr;

pub use error::Error;
pub use key::Key;

#[derive(Debug, Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
pub enum Context {
    BIB_HMAC_SHA2,
    BCB_AES_GCM,
    Unrecognised(u64),
}

impl hardy_cbor::encode::ToCbor for Context {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(match self {
            Self::BIB_HMAC_SHA2 => 1,
            Self::BCB_AES_GCM => 2,
            Self::Unrecognised(v) => v,
        })
    }
}

impl hardy_cbor::decode::FromCbor for Context {
    type Error = hardy_cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
            o.map(|(value, shortest, len)| {
                (
                    match value {
                        1 => Self::BIB_HMAC_SHA2,
                        2 => Self::BCB_AES_GCM,
                        value => Self::Unrecognised(value),
                    },
                    shortest,
                    len,
                )
            })
        })
    }
}
