use core::fmt::{self, Debug, Formatter};

use hardy_async::async_trait;
use hardy_bpv7::{
    Bundle, eid::Eid, extension_editor::ExtensionEditor, reader::Reader, status_report::ReasonCode,
};
use thiserror::Error;

// Name collision with the wire `Bundle` the filter kinds read: the legacy
// filter traits' stored bundle is aliased.
use crate::bundle::{Bundle as StoredBundle, BundleMetadata, WritableMetadata};
use crate::{Arc, Bytes};

mod chain;
mod engine;

pub(crate) use engine::FilterEngine;
/// RFC9171 validity filter - always available, auto-registered by default.
/// Disable auto-registration with `no-rfc9171-autoregister` feature.
pub mod rfc9171;

pub mod pack;
pub mod slots;

/// Bundle validity filter - lifetime and hop-count checks.
pub mod validity;

/// The outcome of a filter invocation, shared across all three kinds.
///
/// `Continue` carries the kind's contribution `T`: a [`slots::MetadataDelta`]
/// for a [`Classifier`], and `()` for a [`Verifier`] (which contributes
/// nothing) and a [`Rewriter`] (whose edits are applied through the editor
/// handle). One enum spans every kind so the drop path — and its status-report
/// reason — is identical everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// What a [`Verifier`] reads: the wire bundle, a [`Reader`] over its block
/// bodies, and the BPA-local record state.
///
/// Built by the engine for each Verifier stage. The getters return the
/// lent views themselves, not borrows of the context; see
/// [reading the bundle](self#reading-the-bundle).
#[derive(Clone, Copy)]
pub struct VerifyContext<'a> {
    bundle: &'a Bundle,
    reader: &'a dyn Reader<'a>,
    metadata: &'a BundleMetadata,
}

impl<'a> VerifyContext<'a> {
    pub(crate) fn new(
        bundle: &'a Bundle,
        reader: &'a dyn Reader<'a>,
        metadata: &'a BundleMetadata,
    ) -> Self {
        Self {
            bundle,
            reader,
            metadata,
        }
    }

    /// The wire bundle: the primary block and the per-block headers.
    ///
    /// This is the parsed [`hardy_bpv7::Bundle`], not the BPA's stored
    /// [`Bundle`](crate::bundle::Bundle) record.
    #[must_use]
    pub fn bundle(&self) -> &'a Bundle {
        self.bundle
    }

    /// The reader over the block bodies: plaintext, or BCB-decrypted with
    /// the node's keys, memoised for the filters that share it.
    #[must_use]
    pub fn reader(&self) -> &'a dyn Reader<'a> {
        self.reader
    }

    /// The BPA-local record state: provenance, the extension-field cache,
    /// and the annotation slots (expiry via [`BundleMetadata::expiry`]).
    ///
    /// The extension-field cache holds the values as received: at Egress
    /// and Deliver, this attempt's Previous Node, Hop Count, and Bundle Age
    /// — and any preceding Rewriter's edits — are read through the reader,
    /// never the cache.
    #[must_use]
    pub fn metadata(&self) -> &'a BundleMetadata {
        self.metadata
    }
}

impl Debug for VerifyContext<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifyContext")
            .field("bundle", &self.bundle.primary.id)
            .finish_non_exhaustive()
    }
}

/// What a [`Classifier`] reads: the wire bundle, a [`Reader`] over its block
/// bodies, and the BPA-local record state as the preceding links of the pass
/// left it.
///
/// Built by the engine for each Classifier, after the previous link's delta
/// is applied. The getters return the lent views themselves, not borrows of
/// the context; see [reading the bundle](self#reading-the-bundle).
#[derive(Clone, Copy)]
pub struct ClassifyContext<'a> {
    bundle: &'a Bundle,
    reader: &'a dyn Reader<'a>,
    metadata: &'a BundleMetadata,
}

impl<'a> ClassifyContext<'a> {
    pub(crate) fn new(
        bundle: &'a Bundle,
        reader: &'a dyn Reader<'a>,
        metadata: &'a BundleMetadata,
    ) -> Self {
        Self {
            bundle,
            reader,
            metadata,
        }
    }

    /// The wire bundle: the primary block and the per-block headers.
    ///
    /// This is the parsed [`hardy_bpv7::Bundle`], not the BPA's stored
    /// [`Bundle`](crate::bundle::Bundle) record.
    #[must_use]
    pub fn bundle(&self) -> &'a Bundle {
        self.bundle
    }

    /// The reader over the block bodies: plaintext, or BCB-decrypted with
    /// the node's keys, memoised for the filters that share it.
    #[must_use]
    pub fn reader(&self) -> &'a dyn Reader<'a> {
        self.reader
    }

    /// The BPA-local record state, including the deltas the preceding
    /// Classifiers of this pass applied.
    #[must_use]
    pub fn metadata(&self) -> &'a BundleMetadata {
        self.metadata
    }
}

impl Debug for ClassifyContext<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClassifyContext")
            .field("bundle", &self.bundle.primary.id)
            .finish_non_exhaustive()
    }
}

/// What a [`Rewriter`] works with: the wire bundle, a [`Reader`] over its
/// block bodies, the BPA-local record state, the [`Boundary`] it runs at,
/// and the scoped [`ExtensionEditor`] its edits go through.
///
/// Built by the engine for each Rewriter, over the wire form as the
/// preceding link left it. The read getters return the lent views
/// themselves, not borrows of the context, so a Rewriter can hold a block
/// it read while it edits; see [reading the bundle](self#reading-the-bundle).
pub struct RewriteContext<'a> {
    bundle: &'a Bundle,
    reader: &'a dyn Reader<'a>,
    metadata: &'a BundleMetadata,
    boundary: Boundary<'a>,
    editor: ExtensionEditor<'a>,
}

impl<'a> RewriteContext<'a> {
    pub(crate) fn new(
        bundle: &'a Bundle,
        reader: &'a dyn Reader<'a>,
        metadata: &'a BundleMetadata,
        boundary: Boundary<'a>,
        editor: ExtensionEditor<'a>,
    ) -> Self {
        Self {
            bundle,
            reader,
            metadata,
            boundary,
            editor,
        }
    }

    /// The wire bundle as this invocation received it: the primary block
    /// and the per-block headers.
    ///
    /// This is the parsed [`hardy_bpv7::Bundle`], not the BPA's stored
    /// [`Bundle`](crate::bundle::Bundle) record. Edits made through
    /// [`editor`](Self::editor) do not show here; the next link sees them.
    #[must_use]
    pub fn bundle(&self) -> &'a Bundle {
        self.bundle
    }

    /// The reader over the block bodies as this invocation received them:
    /// plaintext, or BCB-decrypted with the node's keys.
    #[must_use]
    pub fn reader(&self) -> &'a dyn Reader<'a> {
        self.reader
    }

    /// The BPA-local record state: provenance, the extension-field cache,
    /// and the annotation slots.
    ///
    /// Per-attempt facts — this attempt's per-hop blocks and the preceding
    /// Rewriters' edits — come from the reader: the extension-field cache
    /// holds the values as received.
    #[must_use]
    pub fn metadata(&self) -> &'a BundleMetadata {
        self.metadata
    }

    /// The boundary this invocation runs at, with any boundary-specific
    /// context.
    #[must_use]
    pub fn boundary(&self) -> Boundary<'a> {
        self.boundary
    }

    /// The scoped editor: insert, replace and remove extension blocks only.
    #[must_use]
    pub fn editor(&mut self) -> &mut ExtensionEditor<'a> {
        &mut self.editor
    }

    // Hands the editor back to the engine, which materialises its edits.
    pub(crate) fn into_editor(self) -> ExtensionEditor<'a> {
        self.editor
    }
}

impl Debug for RewriteContext<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RewriteContext")
            .field("bundle", &self.bundle.primary.id)
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

/// A read-only admission check, registrable at any hook. It contributes
/// nothing — it only accepts or drops — and is invoked synchronously,
/// inline on the pipeline task. Verifiers at a hook are order-independent
/// by contract: no ordering is guaranteed among them and there is no
/// cross-talk, so a Verifier must not depend on another filter having run.
///
/// The invocation reads through its [`VerifyContext`]: the wire bundle,
/// its block bodies (plaintext or BCB-decrypted), and the BPA-local record
/// state. See [reading the bundle](self#reading-the-bundle).
pub trait Verifier: Send + Sync {
    /// Inspect the bundle and return [`Verdict::Continue`] to accept or
    /// [`Verdict::Drop`] to reject it.
    fn verify(&self, ctx: &VerifyContext<'_>) -> Verdict;
}

/// An annotating input filter for the Ingress and Originate hooks. Runs
/// sequentially — its [`ClassifyContext`]'s metadata shows the deltas
/// applied by preceding links of the same pass — and contributes a
/// [`slots::MetadataDelta`] the engine applies before the next invocation.
///
/// Node-scoped: it writes metadata this node's own downstream consumes. The
/// returned delta is applied idempotently, never by touching the record
/// directly — the wire bundle and reader, and the BPA-local record state,
/// are deliberately separate views: only the latter ever changes, and only
/// through deltas.
pub trait Classifier: Send + Sync {
    /// Inspect the bundle and return the metadata changes to apply
    /// ([`Verdict::Continue`]) or drop the bundle ([`Verdict::Drop`]).
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<slots::MetadataDelta>;
}

/// An extension-block rewriter. Runs sequentially, per attempt, in memory —
/// the edits are derived fresh each time and never written back to storage —
/// at one of two boundaries, its [`RewriteContext`]'s [`Boundary`]:
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
/// Its edits are confined to *extension* blocks — never the payload — so it
/// runs before the payload's BPSec decrypt at Deliver; the reader decrypts
/// any block it needs to inspect. Each Rewriter sees its predecessors'
/// edits: the engine materialises every invocation's edits into the wire
/// form before the next invocation reads it.
///
/// Removing or replacing a per-hop block (Previous Node, Bundle Age, Hop
/// Count) is permitted and overrides the BPA's own RFC 9171 §4.4 handling:
/// at Egress the fixed per-hop rewrite has already run, so a Rewriter that
/// removes a clockless bundle's Bundle Age transmits a bundle a strict next
/// hop rejects.
///
/// An [`ExtensionEditor`] refusal is the Rewriter's no-match path. A block
/// the Rewriter inserted is a valid target for its own later `replace` or
/// `remove` in the same invocation. An edit the editor accepted that then
/// fails to materialise aborts the process (see the
/// [failure contract](self#failure-and-drop-contract)).
pub trait Rewriter: Send + Sync {
    /// Edit extension blocks through the context's
    /// [`editor`](RewriteContext::editor) — insert/replace/remove only,
    /// never the primary, payload, or BIB/BCB blocks, and never a block
    /// under existing BPSec coverage. The context's bundle and reader are
    /// the wire form as this invocation received it, and its
    /// [`boundary`](RewriteContext::boundary) says where it runs (and, at
    /// Egress, the resolved next hop). Return [`Verdict::Drop`] to abort the
    /// attempt.
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) -> Verdict;
}

// Forwarding impls: a filter built from configuration is held as a boxed
// trait object, and one instance can serve several hooks through an `Arc`.
impl<T: Verifier + ?Sized> Verifier for Box<T> {
    fn verify(&self, ctx: &VerifyContext<'_>) -> Verdict {
        (**self).verify(ctx)
    }
}

impl<T: Verifier + ?Sized> Verifier for Arc<T> {
    fn verify(&self, ctx: &VerifyContext<'_>) -> Verdict {
        (**self).verify(ctx)
    }
}

impl<T: Classifier + ?Sized> Classifier for Box<T> {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<slots::MetadataDelta> {
        (**self).classify(ctx)
    }
}

impl<T: Classifier + ?Sized> Classifier for Arc<T> {
    fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<slots::MetadataDelta> {
        (**self).classify(ctx)
    }
}

impl<T: Rewriter + ?Sized> Rewriter for Box<T> {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) -> Verdict {
        (**self).rewrite(ctx)
    }
}

impl<T: Rewriter + ?Sized> Rewriter for Arc<T> {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) -> Verdict {
        (**self).rewrite(ctx)
    }
}

/// The boundary a [`Rewriter`] is invoked at, with any boundary-specific
/// context.
///
/// Next-hop context is Egress-only — a delivering bundle terminates here and
/// has no next hop — so it rides the variant, letting one trait serve both
/// boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Boundary<'a> {
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
    Continue(Mutation, StoredBundle, Bytes),
    Drop(StoredBundle, Option<ReasonCode>),
}

// Filter traits

/// Read-only filter: can run in parallel with other ReadFilters
#[async_trait]
pub trait ReadFilter: Send + Sync {
    async fn filter(&self, bundle: &StoredBundle, data: &[u8]) -> Result<ReadResult, crate::Error>;
}

/// Read-write filter: runs sequentially, may modify metadata or bundle data
#[async_trait]
pub trait WriteFilter: Send + Sync {
    async fn filter(&self, bundle: &StoredBundle, data: &[u8])
    -> Result<WriteResult, crate::Error>;
}

/// Filter wrapper enum for registration
pub enum Filter {
    Read(Arc<dyn ReadFilter>),
    Write(Arc<dyn WriteFilter>),
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
