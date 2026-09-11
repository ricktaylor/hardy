//! The filter subsystem — the embedder's extension seam on the bundle
//! pipeline.
//!
//! Three payload-free kinds behind one verdict: read-only [`Verifier`]s
//! (any hook), annotating [`Classifier`]s (input hooks, contributing a
//! [`slots::MetadataDelta`]), and extension-block [`Rewriter`]s (output
//! hooks, editing through the scoped [`ScopedEditor`] handle). Filters are
//! registered in [`pack::FilterPack`]s, frozen at
//! [`build()`](crate::builder::BpaBuilder::build), and run inline by the
//! engine at the pipeline's hook positions. The BPA's own checks are
//! pipeline code gated by configuration, never registered filters.
//!
//! # Failure and Drop contract
//!
//! A [`Verdict::Drop`] is policy, never a failure. Its disposition per
//! hook, and the pipeline's recovery when a chain *fails* (the engine
//! could not run its decode pass over the stored bytes — never a
//! filter's verdict):
//!
//! | Hook | `Drop(Some(reason))` | `Drop(None)` | chain failure |
//! |---|---|---|---|
//! | Originate | the reason returns to the caller as `services::Error::Dropped` (pre-store: no report is ever sent) | same, with `None` | `services::Error::Internal` to the caller; nothing was stored |
//! | Ingress | dropped with a deletion report per the bundle's request flags | deleted silently, even when the flags request reporting | resolved as `BlockUnintelligible` — the stored bytes failed the chain's own decode pass |
//! | Egress | dropped with a flag-gated deletion report; the transmission attempt ends | deleted silently | the claim returns to `Waiting` for a fresh routing decision |
//! | Deliver | dropped with a flag-gated deletion report | deleted silently | parked `WaitingForService`, recovered by the next (re-)registration |
//!
//! `bpa.filter.filtered` counts every Drop, `bpa.filter.modified` every
//! applied rewrite, and `bpa.filter.error` every chain failure, all by
//! hook.
//!
//! A [`Rewriter`] execution failure — an invalid edit, or an edit whose
//! materialised bytes do not re-parse — is not a chain failure: a rewrite
//! that was meant to work and has not leaves every subsequent processing
//! step undefined, so the engine fail-stops with a panic naming the
//! failing link's pack-prefixed label (the storage-fault rule).

use hardy_bpv7::{
    block,
    bpsec::{self, bcb, key::KeySource},
    eid::Eid,
    primary_block::PrimaryBlock,
    status_report::ReasonCode,
};
use hardy_cbor::decode::{FromCbor, parse_exact};

pub use self::{
    editor::ScopedEditor,
    pack::FilterPack,
    slots::{MetadataDelta, SlotHandle},
};
use crate::{HashMap, bundle::BundleMetadata};

mod engine;

pub(crate) use engine::ChainOutcome;

/// The scoped extension-block editor and its operation errors.
pub mod editor;

/// Filter packs — the embedder registration surface spliced in by the builder.
pub mod pack;

/// Annotation slots — embedder-private metadata in the classification group.
pub mod slots;

/// The outcome of a filter invocation, shared across all three kinds.
///
/// `Continue` carries the kind's contribution `T`: a [`slots::MetadataDelta`]
/// for a [`Classifier`], and `()` for a [`Verifier`] (which contributes
/// nothing) and a [`Rewriter`] (whose edits are applied through the editor
/// handle). One enum spans every kind so the drop path — and its status-report
/// reason — is identical everywhere.
#[derive(Debug)]
pub enum Verdict<T = ()> {
    /// Accept the bundle, carrying the kind's contribution.
    Continue(T),
    /// Drop the bundle, optionally with a status-report reason code.
    ///
    /// With `Some(reason)`, the Ingress/Egress/Deliver hooks generate an
    /// RFC 9171 §5.10 deletion status report per the bundle's
    /// report-request flags; at Originate no report is ever sent (nothing
    /// is stored yet) and the reason returns to the caller in
    /// [`services::Error::Dropped`](crate::services::Error). With `None`
    /// the drop is silent even when the bundle's flags request reporting —
    /// use `Some(ReasonCode::NoAdditionalInformation)` for a reported drop
    /// with nothing specific to say. The per-hook table in the
    /// [module docs](self#failure-and-drop-contract) is the full contract.
    Drop(Option<ReasonCode>),
}

/// The read handle every filter kind is invoked with: the *wire* bundle, the
/// resident source bytes, the BCB OperationSets, and the key source, bundled
/// into one borrow. The BPA-local record state travels as the invocation's
/// separate `metadata` argument — the wire form and the record's mutable
/// annotations are deliberately never one object.
///
/// The OperationSets are stack-local at the call site (decoded once by
/// `parse()`); the reader lends them to block access, so a filter reads or
/// decrypts blocks without a second parse. Block bodies come back through the
/// bpv7 accessors, which return `None` when the bytes are not resident (the
/// headers-only or streaming case).
pub struct BundleReader<'a> {
    // The *wire* bundle, deliberately: the BPA-local metadata is a separate
    // argument to every filter invocation, so the two states cannot be
    // confused — and the reader is invariant wherever the wire form is.
    bundle: &'a hardy_bpv7::Bundle,
    data: &'a [u8],
    bcb_ops: &'a HashMap<u64, bcb::OperationSet>,
    keys: &'a dyn KeySource,
}

impl<'a> BundleReader<'a> {
    /// Builds a reader over the wire bundle, its resident bytes, the decoded
    /// BCB OperationSets, and the key source. Constructed by the engine at
    /// each hook from the pieces `parse()` produced.
    pub(crate) fn new(
        bundle: &'a hardy_bpv7::Bundle,
        data: &'a [u8],
        bcb_ops: &'a HashMap<u64, bcb::OperationSet>,
        keys: &'a dyn KeySource,
    ) -> Self {
        Self {
            bundle,
            data,
            bcb_ops,
            keys,
        }
    }

    /// The bundle's primary block, decoded into typed fields.
    pub fn primary(&self) -> &'a PrimaryBlock {
        &self.bundle.primary
    }

    /// The block header (type, flags, CRC, BPSec coverage, extents) for a block
    /// number, or `None` when the bundle has no such block. Block *bodies* come
    /// from [`block_data`](Self::block_data).
    pub fn block(&self, block_number: u64) -> Option<&'a block::Block> {
        self.bundle.blocks.get(&block_number)
    }

    /// A block's plaintext bytes: the raw body when unencrypted, or the
    /// BCB-decrypted body (via the OperationSets + key source) when covered.
    /// Same contract as [`hardy_bpv7::bpsec::block_data`].
    ///
    /// `Ok(None)` is the "not available to me" path — the block is absent or
    /// not resident, or it is BCB-covered and no usable key is held (a
    /// Classifier's no-match case). Other BPSec failures propagate. Coverage is
    /// visible up front via [`block`](Self::block)'s `bcb` field, so a filter
    /// never needs the raw ciphertext.
    pub fn block_data(
        &self,
        block_number: u64,
    ) -> Result<Option<block::Payload<'a>>, hardy_bpv7::Error> {
        // `DecryptingReader` supplies the residency pre-check (`Ok(None)`
        // for extents beyond the resident bytes) and the decrypt; the
        // reader-level flattening of NoKey/absent to "not available to
        // me" is this handle's contract.
        match bpsec::DecryptingReader::new(&self.bundle.blocks, self.data, self.bcb_ops, self.keys)
            .block_data(block_number)
        {
            Ok(payload) => Ok(payload),
            Err(hardy_bpv7::Error::InvalidBPSec(bpsec::Error::NoKey))
            | Err(hardy_bpv7::Error::MissingBlock(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// CBOR-decodes a block's plaintext body into `T`, requiring the whole body
    /// to be consumed. Decrypts a covered block first (via [`block_data`]), so
    /// it works uniformly on plaintext and BCB-covered blocks. `Ok(None)` when
    /// the block is absent, not resident, or covered with no usable key.
    ///
    /// [`block_data`]: Self::block_data
    pub fn extract<T>(&self, block_number: u64) -> Result<Option<T>, hardy_bpv7::Error>
    where
        T: FromCbor,
        T::Error: From<hardy_cbor::decode::Error>,
        hardy_bpv7::Error: From<T::Error>,
    {
        match self.block_data(block_number)? {
            Some(payload) => Ok(Some(parse_exact::<T>(payload.as_ref())?)),
            None => Ok(None),
        }
    }
}

/// A read-only admission check, registrable at any hook. It contributes
/// nothing — it only accepts or drops — and is invoked synchronously,
/// inline on the pipeline task. Verifiers at a hook are order-independent
/// by contract: no ordering is guaranteed among them and there is no
/// cross-talk, so a Verifier must not depend on another filter having run.
///
/// The invocation reads the wire bundle through the [`BundleReader`] — the
/// primary block, per-block headers, and block bodies (plaintext or
/// BCB-decrypted) — and the BPA-local record state through the separate
/// `metadata` argument (provenance, extension-field cache, annotation
/// slots; expiry via [`BundleMetadata::expiry`]). The kind is
/// payload-independent by contract.
pub trait Verifier: Send + Sync {
    /// Inspect the bundle and return [`Verdict::Continue`] to accept or
    /// [`Verdict::Drop`] to reject it.
    fn check(&self, reader: &BundleReader<'_>, metadata: &BundleMetadata) -> Verdict;
}

/// An annotating input filter for the Ingress and Originate hooks. Runs
/// sequentially — `metadata` shows the deltas applied by preceding links of
/// the same pass — and contributes a [`slots::MetadataDelta`] the engine
/// applies before the next invocation.
///
/// Node-scoped: it writes metadata this node's own downstream consumes. The
/// returned delta is applied idempotently, never by touching the record
/// directly — the wire bundle (the reader) and the BPA-local record state
/// (`metadata`) are deliberately separate arguments: only the latter ever
/// changes, and only through deltas.
pub trait Classifier: Send + Sync {
    /// Inspect the bundle and return the metadata changes to apply
    /// ([`Verdict::Continue`]) or drop the bundle ([`Verdict::Drop`]).
    fn classify(
        &self,
        reader: &BundleReader<'_>,
        metadata: &BundleMetadata,
    ) -> Verdict<slots::MetadataDelta>;
}

/// An extension-block rewriter. Runs sequentially, per attempt, in memory —
/// the edits are derived fresh each time and never written back to storage —
/// at one of two boundaries, distinguished by the [`RewriteContext`]:
///
/// - **Egress**: prepares the wire form for the resolved next hop
///   (network-scoped — it writes extension blocks the next hops consume).
/// - **Deliver**: strips transport-scoped extension blocks (network QoS,
///   custody — the "transport headers") before a bundle is handed to a local
///   raw-bundle [`Service`](crate::services::Service), so the application
///   receives only content. The chain runs for every local delivery — a
///   Drop verdict applies to `Service` and `Application` deliveries alike
///   — but only the raw-`Service` path observes the rewritten blocks: the
///   payload-only `Application` path receives the decrypted payload alone,
///   so a Rewriter's edits are invisible there.
///
/// It edits *extension* blocks, never the payload, so it runs before the
/// payload's BPSec decrypt at Deliver; it holds the [`KeySource`] to decrypt
/// any extension block it needs to inspect. Each Rewriter sees its
/// predecessors' edits: the engine materialises every invocation's edits into
/// the wire form before the next invocation reads it.
pub trait Rewriter: Send + Sync {
    /// Edit extension blocks through `editor` — insert/replace/remove only,
    /// never the primary, payload, or BIB/BCB blocks, and never a block under
    /// existing BPSec coverage. `context` carries the boundary (and, at
    /// Egress, the resolved next hop). Return [`Verdict::Drop`] to abort the
    /// attempt.
    fn rewrite(
        &self,
        reader: &BundleReader<'_>,
        metadata: &BundleMetadata,
        context: RewriteContext<'_>,
        editor: &mut ScopedEditor<'_>,
    ) -> Verdict;
}

/// The boundary a [`Rewriter`] is invoked at, with any hook-specific context.
///
/// Next-hop context is Egress-only — a delivering bundle terminates here and
/// has no next hop — so it rides the variant rather than the method signature,
/// letting one trait serve both boundaries.
#[derive(Clone, Copy)]
pub enum RewriteContext<'a> {
    /// Preparing the wire form for the resolved `next_hop`, per transmission
    /// attempt.
    Egress {
        /// The adjacency this attempt transmits to — a property of the peer
        /// queue the dispatch decision placed the bundle in.
        next_hop: &'a Eid,
    },
    /// Stripping transport-scoped extension blocks before local delivery.
    /// Runs for every local delivery; the edits are observable only on the
    /// raw-bundle [`Service`](crate::services::Service) path.
    Deliver,
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp};

    use super::*;

    // A present block whose extents fall outside the resident bytes is
    // "not resident": the documented Ok(None), never Err(Altered). Only
    // reachable once the Phase 3 peek seat delivers truncated buffers, but
    // the contract is public today.
    #[test]
    fn block_data_returns_none_for_non_resident_block() {
        let (_, data) = Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
            .with_payload(b"a payload long enough to truncate".as_slice().into())
            .build(CreationTimestamp::now())
            .unwrap();
        let parsed = hardy_bpv7::parse::parse(bytes::Bytes::from(data)).unwrap();
        let bundle = parsed.bundle;

        // Truncate inside the payload block's extents.
        let end = usize::try_from(bundle.blocks[&1].payload_range().end).unwrap();
        let truncated = &parsed.data[..end - 8];

        let bcb_ops = HashMap::default();
        let keys = hardy_bpv7::bpsec::no_keys(&bundle, truncated);
        let reader = BundleReader::new(&bundle, truncated, &bcb_ops, &*keys);
        assert!(matches!(reader.block_data(1), Ok(None)));

        // The whole buffer still reads the block.
        let keys = hardy_bpv7::bpsec::no_keys(&bundle, &parsed.data);
        let reader = BundleReader::new(&bundle, &parsed.data, &bcb_ops, &*keys);
        assert!(matches!(reader.block_data(1), Ok(Some(_))));
    }
}
