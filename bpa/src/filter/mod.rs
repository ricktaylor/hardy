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

use hardy_bpv7::{
    block,
    bpsec::{self, bcb, key::KeySource},
    eid::Eid,
    primary_block::PrimaryBlock,
    status_report::ReasonCode,
};
use hardy_cbor::decode::{FromCbor, parse_exact};

use self::editor::ScopedEditor;
use crate::{
    HashMap,
    bundle::{Bundle, BundleMetadata},
};

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
    Drop(Option<ReasonCode>),
}

/// The read handle every filter kind is invoked with: the bundle, the resident
/// source bytes, the BCB OperationSets, and the key source, bundled into one
/// borrow.
///
/// The OperationSets are stack-local at the call site (decoded once by
/// `parse()`); the reader lends them to block access, so a filter reads or
/// decrypts blocks without a second parse. Block bodies come back through the
/// bpv7 accessors, which return `None` when the bytes are not resident (the
/// headers-only or streaming case).
pub struct BundleReader<'a> {
    bundle: &'a Bundle,
    data: &'a [u8],
    bcb_ops: &'a HashMap<u64, bcb::OperationSet>,
    keys: &'a dyn KeySource,
}

impl<'a> BundleReader<'a> {
    /// Builds a reader over a bundle, its resident bytes, the decoded BCB
    /// OperationSets, and the key source. Constructed by the engine at each
    /// hook from the pieces `parse()` produced.
    pub(crate) fn new(
        bundle: &'a Bundle,
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

    /// The BPA-local metadata (provenance, wire cache, classification), read
    /// through the record's own field privacy.
    pub fn metadata(&self) -> &'a BundleMetadata {
        &self.bundle.metadata
    }

    /// The bundle's primary block, decoded into typed fields.
    pub fn primary(&self) -> &'a PrimaryBlock {
        self.bundle.primary()
    }

    /// The block header (type, flags, CRC, BPSec coverage, extents) for a block
    /// number, or `None` when the bundle has no such block. Block *bodies* come
    /// from [`block_data`](Self::block_data).
    pub fn block(&self, block_number: u64) -> Option<&'a block::Block> {
        self.bundle.bpv7.blocks.get(&block_number)
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
        let bundle = self.bundle;
        match bpsec::block_data(
            block_number,
            &bundle.bpv7.blocks,
            self.data,
            self.bcb_ops,
            self.keys,
        ) {
            Ok(payload) => Ok(Some(payload)),
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

/// A read-only admission check. Runs in parallel with the other Verifiers at
/// its hook (registrable at any hook) and contributes nothing — it only
/// accepts or drops.
///
/// The invocation reads the bundle through the [`BundleReader`] — the primary
/// block, per-block headers, and block bodies (plaintext or BCB-decrypted). The
/// kind is payload-independent by contract.
pub trait Verifier: Send + Sync {
    /// Inspect the bundle and return [`Verdict::Continue`] to accept or
    /// [`Verdict::Drop`] to reject it.
    fn check(&self, reader: &BundleReader<'_>) -> Verdict;
}

/// An annotating input filter for the Ingress and Originate hooks. Runs
/// sequentially — seeing the deltas applied by preceding links of the same
/// pass — and contributes a [`slots::MetadataDelta`] the engine applies before
/// the next invocation.
///
/// Node-scoped: it writes metadata this node's own downstream consumes. The
/// returned delta is applied idempotently, never by touching `bundle.metadata`
/// directly.
pub trait Classifier: Send + Sync {
    /// Inspect the bundle and return the metadata changes to apply
    /// ([`Verdict::Continue`]) or drop the bundle ([`Verdict::Drop`]).
    fn classify(&self, reader: &BundleReader<'_>) -> Verdict<slots::MetadataDelta>;
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
///   receives only content. Only the raw-`Service` path sees blocks at all;
///   the payload-only `Application` path never does.
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
        /// The next hop the dispatch decision resolved for this attempt.
        next_hop: &'a Eid,
    },
    /// Stripping transport-scoped extension blocks before local delivery to a
    /// raw-bundle [`Service`](crate::services::Service).
    Deliver,
}
