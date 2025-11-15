use super::*;

pub mod bcb;
pub mod bib;
pub mod encryptor;
pub mod key;
pub mod signer;

mod error;
mod parse;

#[cfg(feature = "rfc9173")]
pub mod rfc9173;

use error::CaptureFieldErr;

pub use error::Error;
pub use key::Key;

#[derive(Debug, Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
pub enum Context {
    #[cfg(feature = "rfc9173")]
    BIB_HMAC_SHA2,
    #[cfg(feature = "rfc9173")]
    BCB_AES_GCM,
    Unrecognised(u64),
}

impl hardy_cbor::encode::ToCbor for Context {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(match self {
            #[cfg(feature = "rfc9173")]
            Self::BIB_HMAC_SHA2 => &1,
            #[cfg(feature = "rfc9173")]
            Self::BCB_AES_GCM => &2,
            Self::Unrecognised(v) => v,
        })
    }
}

impl hardy_cbor::decode::FromCbor for Context {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data).map(|(value, shortest, len)| {
            (
                match value {
                    #[cfg(feature = "rfc9173")]
                    1 => Self::BIB_HMAC_SHA2,
                    #[cfg(feature = "rfc9173")]
                    2 => Self::BCB_AES_GCM,
                    value => Self::Unrecognised(value),
                },
                shortest,
                len,
            )
        })
    }
}

pub trait BlockSet<'a> {
    fn block(&self, block_number: u64) -> Option<&block::Block>;

    fn block_payload(&'a self, block_number: u64) -> Option<block::Payload<'a>>;
}
