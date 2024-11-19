use super::*;
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub mod bcb;
mod bcb_aes_gcm;
pub mod bib;
mod bib_hmac_sha2;
mod error;
mod parse;
mod rfc9173;

use error::CaptureFieldErr;

pub use error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
pub enum Context {
    BIB_HMAC_SHA2,
    BCB_AES_GCM,
    Unrecognised(u64),
}

impl std::fmt::Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Context::BIB_HMAC_SHA2 => write!(f, "BIB-HMAC-SHA2"),
            Context::BCB_AES_GCM => write!(f, "BCB-AES-GCM"),
            Context::Unrecognised(v) => write!(f, "Unrecognised {v}"),
        }
    }
}

impl cbor::encode::ToCbor for Context {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        encoder.emit(match self {
            Self::BIB_HMAC_SHA2 => 1,
            Self::BCB_AES_GCM => 2,
            Self::Unrecognised(v) => v,
        })
    }
}

impl cbor::decode::FromCbor for Context {
    type Error = cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
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

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub enum KeyMaterial {
    SymmetricKey(Box<[u8]>),
    PrivateKey,
}

pub struct OperationArgs<'a> {
    pub bpsec_source: &'a Eid,
    pub target: &'a block::Block,
    pub target_number: u64,
    pub source: &'a block::Block,
    pub source_number: u64,
    pub bundle: &'a Bundle,
    pub canonical_primary_block: bool,
    pub bundle_data: &'a [u8],
}
