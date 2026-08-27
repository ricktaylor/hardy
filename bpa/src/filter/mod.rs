use hardy_async::async_trait;
use hardy_bpv7::{
    block,
    bpsec::{self, bcb, key::KeySource},
    eid::Eid,
    primary_block::PrimaryBlock,
    status_report::ReasonCode,
};
use hardy_cbor::decode::{FromCbor, parse_exact};
use thiserror::Error;

use crate::bundle::{Bundle, BundleMetadata, WritableMetadata};
use crate::{Arc, Bytes, HashMap};

mod chain;
mod engine;

pub(crate) use engine::FilterEngine;
/// RFC9171 validity filter - always available, auto-registered by default.
/// Disable auto-registration with `no-rfc9171-autoregister` feature.
pub mod rfc9171;

/// Annotation slots — embedder-private metadata in the classification group.
pub mod slots;

/// Bundle validity filter - lifetime and hop-count checks.
pub mod validity;

/// Errors related to filter registration and dependency management.
#[derive(Debug, Error)]
pub enum Error {
    /// A filter with the given name is already registered.
    #[error("Filter with name '{0}' already exists")]
    AlreadyExists(String),

    /// A filter declares a dependency on another filter that has not been registered.
    #[error("Filter dependency '{0}' not found")]
    DependencyNotFound(String),

    /// Cannot remove a filter because other filters depend on it.
    #[error("Filter '{0}' has dependants: {1:?}")]
    HasDependants(String, Vec<String>),
}

/// Outcome of a read-only filter evaluation.
#[derive(Debug, Default)]
pub enum ReadResult {
    /// Allow the bundle to proceed to the next filter or processing stage.
    #[default]
    Continue,
    /// Drop the bundle, optionally providing a status-report reason code.
    Drop(Option<ReasonCode>),
}

/// Outcome of a read-write filter evaluation, which may modify the bundle.
#[derive(Debug)]
pub enum WriteResult {
    /// Continue processing, optionally with modified metadata and/or bundle data
    /// - (None, None): no change
    /// - (Some(meta), None): metadata changed, bundle bytes unchanged
    /// - (None, Some(data)): bundle bytes changed (rare)
    /// - (Some(meta), Some(data)): both changed
    Continue(Option<WritableMetadata>, Option<Vec<u8>>),
    /// Drop the bundle, optionally providing a status-report reason code.
    Drop(Option<ReasonCode>),
}

/// Tracks whether filters modified the bundle or its metadata.
#[derive(Default)]
pub struct Mutation {
    pub data: bool,
    pub metadata: bool,
}

/// Result of executing the filter chain on a bundle.
#[allow(clippy::large_enum_variant)]
pub enum ExecResult {
    Continue(Mutation, Bundle, Bytes),
    Drop(Bundle, Option<ReasonCode>),
}

// Filter traits

/// Read-only filter: can run in parallel with other ReadFilters
#[async_trait]
pub trait ReadFilter: Send + Sync {
    async fn filter(&self, bundle: &Bundle, data: &[u8]) -> Result<ReadResult, crate::Error>;
}

/// Read-write filter: runs sequentially, may modify metadata or bundle data
#[async_trait]
pub trait WriteFilter: Send + Sync {
    async fn filter(&self, bundle: &Bundle, data: &[u8]) -> Result<WriteResult, crate::Error>;
}

/// Filter wrapper enum for registration
pub enum Filter {
    Read(Arc<dyn ReadFilter>),
    Write(Arc<dyn WriteFilter>),
}

// ---------------------------------------------------------------------------
// Phase 2 filter kinds — the committed extension-API traits and their verdict.
//
// These are the successors to `ReadFilter`/`WriteFilter`: three payload-free,
// byte-pure kinds behind one verdict. Added here unconsumed — the engine still
// runs the old traits until the C3 swap wires these into the dispatcher.
// ---------------------------------------------------------------------------

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
    #[allow(dead_code)] // wired by the engine when the C3 swap lands
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
        &self.bundle.bundle.primary
    }

    /// The block header (type, flags, CRC, BPSec coverage, extents) for a block
    /// number, or `None` when the bundle has no such block. Block *bodies* come
    /// from [`block_data`](Self::block_data).
    pub fn block(&self, block_number: u64) -> Option<&'a block::Block> {
        self.bundle.bundle.blocks.get(&block_number)
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
            &bundle.bundle.blocks,
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
/// any extension block it needs to inspect.
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

/// The scoped extension-block editor handed to a [`Rewriter`].
///
/// It exposes insert/replace/remove of *extension* blocks only, making
/// payload/primary/BIB/BCB immutability a compile-time property rather than a
/// review promise, and refusing edits to blocks under existing BPSec coverage.
/// The concrete operation set — and the plumbing that constructs one over the
/// bundle being transmitted — lands with the filter registration surface (the
/// remainder of C2); this is the handle type the [`Rewriter`] trait is defined
/// against.
pub struct ScopedEditor<'a> {
    // Placeholder: the operation surface and its backing editor land with the
    // registration step. Carries the borrow the real handle will hold.
    #[allow(dead_code)]
    bundle: core::marker::PhantomData<&'a mut Bundle>,
}

/// Hook points in bundle processing
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
#[derive(Debug)]
pub enum Hook {
    Ingress,
    Deliver,
    Originate,
    Egress,
}

impl Hook {
    /// Returns the lowercase string label for this hook point (e.g. `"ingress"`).
    pub fn label(&self) -> &'static str {
        match self {
            Hook::Ingress => "ingress",
            Hook::Deliver => "deliver",
            Hook::Originate => "originate",
            Hook::Egress => "egress",
        }
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Hook {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "ingress" => Ok(Hook::Ingress),
            "deliver" => Ok(Hook::Deliver),
            "originate" => Ok(Hook::Originate),
            "egress" => Ok(Hook::Egress),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &["ingress", "deliver", "originate", "egress"],
            )),
        }
    }
}
