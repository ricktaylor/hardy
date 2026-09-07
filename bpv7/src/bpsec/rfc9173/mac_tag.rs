use alloc::boxed::Box;
use core::fmt;

use hardy_cbor::encode::{Bytes, Encoder, ToCbor};
use hmac::{EagerHash, Hmac, Mac};
use subtle::ConstantTimeEq;

/// A wire-carried MAC tag whose bytes are reachable only inside this
/// module, so every comparison a caller can write is constant-time.
#[derive(Debug, Clone)]
pub struct MacTag(Box<[u8]>);

impl MacTag {
    /// Wraps a tag read from the wire.
    pub(crate) fn from_bytes(bytes: Box<[u8]>) -> Self {
        Self(bytes)
    }

    /// Finalizes a freshly computed MAC into its tag.
    pub(crate) fn from_mac<D: EagerHash>(mac: Hmac<D>) -> Self {
        Self(Box::from(mac.finalize().into_bytes().as_ref()))
    }

    /// Constant-time check of a freshly computed MAC against this tag.
    #[must_use]
    pub(crate) fn verify<D: EagerHash>(&self, mac: Hmac<D>) -> bool {
        mac.verify_slice(&self.0).is_ok()
    }
}

// Wire-vs-wire equality (bundle comparison). Constant-time so tag
// equality can never become a timing oracle.
impl PartialEq for MacTag {
    fn eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl Eq for MacTag {}

impl fmt::LowerHex for MacTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl ToCbor for MacTag {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(&Bytes(&self.0));
    }
}
