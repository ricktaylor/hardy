use super::*;

pub(crate) mod registry;

/// A provider of cryptographic keys that can be queried based on bundle context.
pub trait KeyProvider: Send + Sync {
    /// Returns a KeySource that can provide keys for this bundle context.
    /// The source EID and operations are passed later when keys() is called.
    fn key_source(
        &self,
        bundle: &hardy_bpv7::bundle::Bundle,
        data: &[u8],
    ) -> Box<dyn hardy_bpv7::bpsec::key::KeySource>;
}
