//! The filter subsystem — the embedder's extension seam on the bundle
//! pipeline.
//!
//! Three kinds: read-only [`Verifier`]s (Ingress, Originate and Deliver),
//! annotating [`Classifier`]s (input hooks, contributing a
//! [`slots::MetadataDelta`]), and extension-block [`Rewriter`]s (output
//! hooks, editing through bpv7's scoped [`ExtensionEditor`]). Verifiers and
//! Classifiers return a [`Verdict`]; a Rewriter returns nothing. Filters are
//! registered in [`pack::FilterPack`]s, frozen at
//! [`build()`](crate::builder::BpaBuilder::build), and run inline by the
//! engine at the pipeline's hook positions. The BPA's own checks are
//! pipeline code gated by configuration, never registered filters.
//!
//! A registered filter is trusted code: it reads block plaintext —
//! BCB-decrypted with the node's keys on request, the payload's included
//! whenever it is resident — and only a Rewriter's *edits* are scoped, to
//! extension blocks.
//!
//! # Reading the bundle
//!
//! Every invocation receives its kind's context — [`VerifyContext`],
//! [`ClassifyContext`] or [`RewriteContext`] — lending the *wire* bundle
//! (the primary block and the per-block headers), a [`Reader`] over its
//! block bodies, and the BPA-local record state, each through its own
//! getter: the wire form and the record's annotations are never one
//! object. The reader is a
//! [`DecryptingReader`](hardy_bpv7::bpsec::DecryptingReader) over the
//! node's keys, so a BCB-covered block reads as its plaintext when a key is
//! held, and otherwise as the
//! [`Availability`](hardy_bpv7::reader::Availability) state that says why
//! not (not resident, no key, not decryptable). It memoises, decrypting a
//! covered block at most once for the filters that share it: one reader
//! serves a whole input pass and the Verifiers that close the Deliver
//! chain, and each Rewriter gets its own, since an edit replaces the bytes.
//! [`ReaderExt::extract`](hardy_bpv7::reader::ReaderExt::extract)
//! CBOR-decodes a block body. A Verifier's and a Classifier's context also
//! lends the payload's resident prefix, the declared peek among it, through
//! [`VerifyContext::payload_peek`].
//!
//! # Failure and Drop contract
//!
//! A [`Verdict::Drop`] is policy, never a failure. Only Verifiers and
//! Classifiers return one, so nothing at Egress drops a bundle. Its
//! disposition per hook, and what a chain *failure* means there (the engine
//! could not decode the bytes; never a filter's verdict):
//!
//! | Hook | `Drop(Some(reason))` | `Drop(None)` | chain failure |
//! |---|---|---|---|
//! | Originate | the reason returns to the caller as `services::Error::Dropped` (pre-store: no report is ever sent) | same, with `None` | none — like Ingress, the chain runs on the door's own header decode |
//! | Ingress | dropped before anything is stored, with one reception + deletion report per the bundle's request flags | the same, without the deletion assertion even when the flags request one (a requested reception report is still sent) | none — the chain decodes nothing itself: it runs on the gate's header decode, whose failures are the header pass's |
//! | Deliver | dropped with a flag-gated deletion report | deleted silently | fatal (below) |
//!
//! At Egress the chain runs on the per-hop rewrite's output, and that
//! rewrite decodes the stored bytes first (an undecodable stored bundle
//! parks `Waiting` there, before the chain runs), so an Egress chain
//! failure means the BPA's own rebuild produced bytes that do not decode.
//!
//! `bpa.filter.filtered` counts every Drop and `bpa.filter.modified` every
//! applied rewrite, both by hook.
//!
//! The output chains have no failure path. Their bytes were validated at
//! ingress and are read back from storage, so an Egress or Deliver chain
//! that cannot decode them has met a BPA bug or storage corruption. A
//! [`Rewriter`] execution failure — an invalid edit, or an edit whose
//! materialised bytes do not re-parse — means a rewrite that was meant to
//! work has not. Either leaves every subsequent processing step undefined,
//! so the engine panics (naming the failing link's pack-prefixed label, for
//! a Rewriter), and the panic aborts the process — the fail-fast rule,
//! applied by analogy with a storage fault. The bundle stays stored and
//! restart recovery re-queues it, so a deterministic failure recurs on every
//! restart: the node crash-loops until the bundle is removed, the accepted
//! cost of failing fast. The [`ExtensionEditor`] refuses at call time the
//! edits it knows a receiver would reject, so a Rewriter that treats refusals
//! as its no-match path meets those as refusals.

use core::fmt::{self, Debug, Formatter};

use hardy_bpv7::{
    Bundle, eid::Eid, extension_editor::ExtensionEditor, reader::Reader, status_report::ReasonCode,
};

use crate::{Arc, bundle::BundleMetadata};

mod engine;

pub(crate) use self::engine::ChainOutcome;

pub mod pack;
pub mod slots;

/// The outcome of a [`Verifier`] or [`Classifier`] invocation.
///
/// `Continue` carries the kind's contribution `T`: a [`slots::MetadataDelta`]
/// for a Classifier, and `()` for a Verifier (which contributes nothing). One
/// enum spans both kinds so the drop path — and its status-report reason — is
/// identical everywhere. A [`Rewriter`] has no verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verdict<T = ()> {
    /// Accept the bundle, carrying the kind's contribution.
    Continue(T),
    /// Drop the bundle, optionally with a status-report reason code.
    ///
    /// With `Some(reason)`, the Ingress and Deliver hooks generate an
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
    peek: Option<&'a [u8]>,
}

impl<'a> VerifyContext<'a> {
    pub(crate) fn new(
        bundle: &'a Bundle,
        reader: &'a dyn Reader<'a>,
        metadata: &'a BundleMetadata,
        peek: Option<&'a [u8]>,
    ) -> Self {
        Self {
            bundle,
            reader,
            metadata,
            peek,
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

    /// The payload's resident prefix: every byte of the payload block's data
    /// that has arrived, whether or not the payload is all resident. Where
    /// the payload is resident, it is all of it.
    ///
    /// At the input hooks the door holds at least the first min(P, payload
    /// length) bytes before the chain runs, P being the largest payload peek
    /// declared at that hook (see [`FilterPack`](pack::FilterPack)); an empty
    /// slice means no payload byte has arrived, or the payload is empty, which
    /// a filter that declared a peek can tell apart. There the bytes may
    /// precede the payload's CRC and BIB checks, as may block 1 read through
    /// [`reader`](Self::reader); both settle before the bundle commits or its
    /// route executes. Treat the peek as unauthenticated input: classify or
    /// drop on it, but do not let it drive durable or attributable effects,
    /// which a forged prefix on a genuinely signed bundle would drive under
    /// its source's name.
    ///
    /// `None` for a payload a BCB covers, as no filter reads an encrypted
    /// payload, and for a fragment past the payload's start, whose bytes are
    /// not the payload's prefix: the reassembled bundle is peeked when it
    /// re-crosses the Ingress gate.
    #[must_use]
    pub fn payload_peek(&self) -> Option<&'a [u8]> {
        self.peek
    }

    /// The BPA-local record state: provenance, the extension-field cache,
    /// and the annotation slots (expiry via [`BundleMetadata::expiry`]).
    ///
    /// The extension-field cache holds the values as received: at Deliver,
    /// the preceding Rewriters' edits are read through the reader, never
    /// the cache.
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
    peek: Option<&'a [u8]>,
}

impl<'a> ClassifyContext<'a> {
    pub(crate) fn new(
        bundle: &'a Bundle,
        reader: &'a dyn Reader<'a>,
        metadata: &'a BundleMetadata,
        peek: Option<&'a [u8]>,
    ) -> Self {
        Self {
            bundle,
            reader,
            metadata,
            peek,
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

    /// The payload's resident prefix, as
    /// [`VerifyContext::payload_peek`] lends it.
    #[must_use]
    pub fn payload_peek(&self) -> Option<&'a [u8]> {
        self.peek
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
    /// The preceding Rewriters' edits come from the reader: the
    /// extension-field cache holds the values as received. At Egress the
    /// Rewriters run before the BPA writes this hop's Previous Node, Hop
    /// Count and Bundle Age, so both show those blocks as received.
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

/// A read-only admission check, registrable at the Ingress, Originate and
/// Deliver hooks. It contributes nothing — it only accepts or drops — and
/// is invoked synchronously, inline on the pipeline task. Verifiers at a
/// hook are order-independent by contract: no ordering is guaranteed among
/// them and there is no cross-talk, so a Verifier must not depend on
/// another filter having run.
///
/// There is no Egress Verifier. The one input unique to Egress is the next
/// hop, and dropping a bundle is the wrong answer to "not via this hop":
/// per-adjacency policy belongs to routing, content policy to an Ingress
/// Verifier.
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
///   receives only content. The chain runs for every local delivery, but
///   only the raw-`Service` path observes the rewritten blocks: the
///   payload-only `Application` path receives the decrypted payload alone,
///   so a Rewriter's edits are invisible there.
///
/// Its edits are confined to *extension* blocks — never the payload — so it
/// runs before the payload's BPSec decrypt at Deliver; the reader decrypts
/// any block it needs to inspect. Each Rewriter sees its predecessors'
/// edits: the engine materialises every invocation's edits into the wire
/// form before the next invocation reads it.
///
/// At Egress the Rewriters run before the BPA's per-hop writes (RFC 9171
/// §5.4), which supersede their edits to the blocks those writes cover: the
/// Previous Node always; the Hop Count when the bundle arrived from a peer
/// with one this node could read and can update (origination is not a hop,
/// so the originating node never writes it); and the Bundle Age when the
/// bundle arrived with one or has no creation clock. A per-hop block a
/// Rewriter adds or edits where the BPA writes none travels as the Rewriter
/// left it.
///
/// A Rewriter has no verdict: it edits the bundle or leaves it as it is,
/// and never drops it. An [`ExtensionEditor`] refusal is the Rewriter's
/// no-match path. A block the Rewriter inserted is a valid target for its
/// own later `replace` or `remove` in the same invocation. An edit the
/// editor accepted that then fails to materialise aborts the process (see
/// the [failure contract](self#failure-and-drop-contract)).
pub trait Rewriter: Send + Sync {
    /// Edit extension blocks through the context's
    /// [`editor`](RewriteContext::editor) — insert/replace/remove only,
    /// never the primary, payload, or BIB/BCB blocks, and never a block
    /// under existing BPSec coverage. The context's bundle and reader are
    /// the wire form as this invocation received it, and its
    /// [`boundary`](RewriteContext::boundary) says where it runs (and, at
    /// Egress, the resolved next hop). The engine materialises the
    /// accepted edits when it returns.
    fn rewrite(&self, ctx: &mut RewriteContext<'_>);
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
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        (**self).rewrite(ctx)
    }
}

impl<T: Rewriter + ?Sized> Rewriter for Arc<T> {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
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
