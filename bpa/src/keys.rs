use alloc::boxed::Box;

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
    /// (primary block + blocks map). The decoded extension fields
    /// (`previous_node`, `age`, `hop_count`) are *not* available here —
    /// keyed BPSec hasn't run yet — so an implementation that needs them
    /// must decode the relevant extension blocks itself.
    ///
    /// [`KeySource`]: hardy_bpv7::bpsec::key::KeySource
    fn key_source(
        &self,
        bundle: &hardy_bpv7::Bundle,
        data: &[u8],
    ) -> Box<dyn hardy_bpv7::bpsec::key::KeySource>;
}

/// The default [`KeyProvider`]: no keys, every BPSec lookup is NoKey.
pub(crate) struct NullKeyProvider;

impl KeyProvider for NullKeyProvider {
    fn key_source(
        &self,
        _bundle: &hardy_bpv7::Bundle,
        _data: &[u8],
    ) -> Box<dyn hardy_bpv7::bpsec::key::KeySource> {
        Box::new(hardy_bpv7::bpsec::key::KeySet::EMPTY)
    }
}
