//! Fast CBOR pre-checks before full bundle parsing.

use hardy_bpv7::Error as BpvError;
use hardy_cbor::decode::Error as CborError;

use crate::Bytes;

/// Reject obviously malformed data before attempting a full parse.
///
/// Checks the first byte to catch empty payloads, BPv6 bundles, and
/// data that cannot be a CBOR array (the required outer structure of a BPv7 bundle).
#[inline(always)]
pub(crate) fn precheck(data: &Bytes) -> Result<(), BpvError> {
    match data.first() {
        None => Err(BpvError::InvalidCBOR(CborError::NeedMoreData(1))),
        Some(0x06) => Err(BpvError::InvalidCBOR(CborError::IncorrectType(
            "BPv7 bundle".to_string(),
            "Possible BPv6 bundle".to_string(),
        ))),
        Some(0x80..=0x9F) => Ok(()),
        Some(_) => Err(BpvError::InvalidCBOR(CborError::IncorrectType(
            "BPv7 bundle".to_string(),
            "Invalid CBOR".to_string(),
        ))),
    }
}
