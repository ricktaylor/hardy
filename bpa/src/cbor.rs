//! Fast CBOR pre-checks before full bundle parsing.

use crate::Bytes;

/// Rejection reason from the CBOR precheck.
///
/// Uses `&'static str` to avoid heap allocation on the error path — under
/// adversarial traffic every malformed packet would otherwise allocate.
#[derive(Debug)]
pub(crate) enum PrecheckError {
    Empty,
    PossibleBpv6,
    NotCborArray,
}

impl core::fmt::Display for PrecheckError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("empty payload"),
            Self::PossibleBpv6 => f.write_str("possible BPv6 bundle"),
            Self::NotCborArray => f.write_str("not a CBOR array"),
        }
    }
}

/// Reject obviously malformed data before attempting a full parse.
///
/// Checks the first byte to catch empty payloads, BPv6 bundles, and
/// data that cannot be a CBOR array (the required outer structure of a BPv7 bundle).
#[inline(always)]
pub(crate) fn precheck(data: &Bytes) -> Result<(), PrecheckError> {
    match data.first() {
        None => Err(PrecheckError::Empty),
        Some(0x06) => Err(PrecheckError::PossibleBpv6),
        Some(0x80..=0x9F) => Ok(()),
        Some(_) => Err(PrecheckError::NotCborArray),
    }
}
