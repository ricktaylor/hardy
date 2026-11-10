use alloc::boxed::Box;

use hardy_bpv7::{
    Bundle,
    block::Type,
    bpsec::key::{KeySet, KeySource},
};

/// Selects the key material to use for a given bundle.
///
/// The flow/context-level selector: it may inspect the bundle — its source, or
/// an extension block carrying a security context — to choose which set of keys
/// applies to this bundle's flow, and returns a [`KeySource`] scoped to that
/// choice. Distinct from [`KeySource`], which resolves an individual key from
/// the security *source* EID — the node that applied the protection, which under
/// RFC 9172 need not be the bundle source. The split lets key material be chosen
/// per flow while keys are still matched per security block.
///
/// [`KeySource`]: hardy_bpv7::bpsec::key::KeySource
pub trait KeyProvider: Send + Sync {
    /// Returns the [`KeySource`] to use for this bundle.
    ///
    /// The bundle argument is the structural [`hardy_bpv7::Bundle`]
    /// (primary block + blocks map), and `data` its resident bytes. At an
    /// input door `data` may be only a prefix — the header blocks and
    /// whatever of the payload has arrived — so a block's extent can run
    /// past its end. At the output doors it is a prefix of the stored bundle
    /// that reaches the payload's data, with any declared Deliver peek, so
    /// every block the output stages edit or read lies within it. The decoded
    /// extension fields (`previous_node`, `age`, `hop_count`) are *not*
    /// available here — keyed BPSec hasn't run yet — so an implementation
    /// that needs them must decode the relevant extension blocks itself.
    ///
    /// [`KeySource`]: hardy_bpv7::bpsec::key::KeySource
    fn key_source(&self, bundle: &Bundle, data: &[u8]) -> Box<dyn KeySource>;
}

/// The key source for one pass over `bundle` and its resident `data`. A
/// bundle without a BCB never consults keys — decrypted reads exist only for
/// blocks under a BCB, and no stage adds one — so it skips the provider
/// round-trip. Every door derives its keys here, so the paths cannot
/// diverge.
pub(crate) fn derive_keys(
    bundle: &Bundle,
    data: &[u8],
    key_provider: &dyn KeyProvider,
) -> Box<dyn KeySource> {
    if bundle
        .blocks
        .values()
        .any(|block| block.block_type == Type::BlockSecurity)
    {
        key_provider.key_source(bundle, data)
    } else {
        Box::new(KeySet::EMPTY)
    }
}

/// The default [`KeyProvider`]: no keys, every BPSec lookup is NoKey.
pub(crate) struct NullKeyProvider;

impl KeyProvider for NullKeyProvider {
    fn key_source(&self, _bundle: &Bundle, _data: &[u8]) -> Box<dyn KeySource> {
        Box::new(KeySet::EMPTY)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::{boxed::Box, vec::Vec};

    use hardy_bpv7::bpsec::key::{Key, KeySet, KeySource};

    use super::KeyProvider;

    /// A [`KeyProvider`] of fixed keys, for a pass that needs them.
    pub(crate) struct Keys(pub(crate) Vec<Key>);

    impl KeyProvider for Keys {
        fn key_source(&self, _bundle: &hardy_bpv7::Bundle, _data: &[u8]) -> Box<dyn KeySource> {
            Box::new(KeySet::new(self.0.clone()))
        }
    }
}
