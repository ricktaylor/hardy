//! The filter subsystem — the embedder's extension seam on the bundle
//! pipeline.
//!
//! Three kinds behind one verdict: read-only [`Verifier`]s (any hook),
//! annotating [`Classifier`]s (input hooks, contributing a
//! [`slots::MetadataDelta`]), and extension-block [`Rewriter`]s (output
//! hooks, editing through bpv7's scoped [`ExtensionEditor`]). Filters are
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
//! Every invocation receives the *wire* bundle — the primary block and the
//! per-block headers — and a [`Reader`] over its block bodies, with the
//! BPA-local record state as a separate `metadata` argument: the wire form
//! and the record's annotations are never one object. The reader is a
//! [`DecryptingReader`](hardy_bpv7::bpsec::DecryptingReader) over the
//! node's keys, so a BCB-covered block reads as its plaintext when a key is
//! held, and otherwise as the
//! [`Availability`](hardy_bpv7::reader::Availability) state that says why
//! not (not resident, no key, not decryptable). It memoises, decrypting a
//! covered block at most once for the filters that share it: one reader
//! serves a whole input pass and the Verifiers that close an output chain,
//! and each Rewriter gets its own, since an edit replaces the bytes.
//! [`ReaderExt::extract`](hardy_bpv7::reader::ReaderExt::extract)
//! CBOR-decodes a block body.
//!
//! # Failure and Drop contract
//!
//! A [`Verdict::Drop`] is policy, never a failure. Its disposition per
//! hook, and the pipeline's recovery when an output chain *fails* (the
//! engine could not decode the stored bytes — never a filter's verdict;
//! the input chains decode nothing themselves, so they cannot fail):
//!
//! | Hook | `Drop(Some(reason))` | `Drop(None)` | chain failure |
//! |---|---|---|---|
//! | Originate | the reason returns to the caller as `services::Error::Dropped` (pre-store: no report is ever sent) | same, with `None` | none — like Ingress, the chain runs on the door's own header decode |
//! | Ingress | dropped before anything is stored, with one reception + deletion report per the bundle's request flags | the same, without the deletion assertion even when the flags request one (a requested reception report is still sent) | none — the chain decodes nothing itself: it runs on the gate's header decode, whose failures are the header pass's |
//! | Egress | dropped with a flag-gated deletion report; the transmission attempt ends | deleted silently | the claim returns to `Waiting` for a fresh routing decision |
//! | Deliver | dropped with a flag-gated deletion report | deleted silently | parked `WaitingForService`, recovered by the next (re-)registration |
//!
//! At Egress the chain runs on the per-hop rewrite's output, and that
//! rewrite decodes the stored bytes first (an undecodable stored bundle
//! parks `Waiting` there, before the chain runs), so an Egress chain
//! failure means the BPA's own rebuild produced bytes that do not decode.
//!
//! `bpa.filter.filtered` counts every Drop, `bpa.filter.modified` every
//! applied rewrite, and `bpa.filter.error` every chain failure, all by
//! hook.
//!
//! A [`Rewriter`] execution failure — an invalid edit, or an edit whose
//! materialised bytes do not re-parse — is not a chain failure: a rewrite
//! that was meant to work and has not leaves every subsequent processing
//! step undefined, so the engine panics naming the failing link's
//! pack-prefixed label, and the panic aborts the process — the fail-fast
//! rule, applied by analogy with a storage fault. The bundle stays stored
//! and restart recovery re-queues it, so a deterministic failure recurs on
//! every restart: the node crash-loops until the bundle is removed, the
//! accepted cost of failing fast. The [`ExtensionEditor`]
//! refuses at call time the edits it knows a receiver would reject, so a
//! Rewriter that treats refusals as its no-match path meets those as
//! refusals.

use hardy_bpv7::{
    Bundle, eid::Eid, extension_editor::ExtensionEditor, reader::Reader, status_report::ReasonCode,
};

use crate::bundle::BundleMetadata;

mod engine;

pub(crate) use engine::ChainOutcome;

pub mod pack;
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

/// A read-only admission check, registrable at any hook. It contributes
/// nothing — it only accepts or drops — and is invoked synchronously,
/// inline on the pipeline task. Verifiers at a hook are order-independent
/// by contract: no ordering is guaranteed among them and there is no
/// cross-talk, so a Verifier must not depend on another filter having run.
///
/// The invocation reads the wire bundle — the primary block and per-block
/// headers through `bundle`, block bodies (plaintext or BCB-decrypted)
/// through `reader` — and the BPA-local record state through the separate
/// `metadata` argument (provenance, extension-field cache, annotation
/// slots; expiry via [`BundleMetadata::expiry`]). The extension-field
/// cache holds the values as received: at Egress and Deliver, this
/// attempt's Previous Node, Hop Count, and Bundle Age — and any preceding
/// Rewriter's edits — are read through the reader, never the cache. See
/// [reading the bundle](self#reading-the-bundle).
pub trait Verifier: Send + Sync {
    /// Inspect the bundle and return [`Verdict::Continue`] to accept or
    /// [`Verdict::Drop`] to reject it.
    fn verify<'a>(
        &self,
        bundle: &Bundle,
        reader: &'a dyn Reader<'a>,
        metadata: &BundleMetadata,
    ) -> Verdict;
}

/// An annotating input filter for the Ingress and Originate hooks. Runs
/// sequentially — `metadata` shows the deltas applied by preceding links of
/// the same pass — and contributes a [`slots::MetadataDelta`] the engine
/// applies before the next invocation.
///
/// Node-scoped: it writes metadata this node's own downstream consumes. The
/// returned delta is applied idempotently, never by touching the record
/// directly — the wire bundle (`bundle` and `reader`) and the BPA-local
/// record state (`metadata`) are deliberately separate arguments: only the
/// latter ever changes, and only through deltas.
pub trait Classifier: Send + Sync {
    /// Inspect the bundle and return the metadata changes to apply
    /// ([`Verdict::Continue`]) or drop the bundle ([`Verdict::Drop`]).
    fn classify<'a>(
        &self,
        bundle: &Bundle,
        reader: &'a dyn Reader<'a>,
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
/// Per-attempt facts — this attempt's per-hop blocks and its predecessors'
/// edits — come from the reader: `metadata.extensions` holds the values as
/// received.
///
/// An [`ExtensionEditor`] refusal is the Rewriter's no-match path. A block
/// the Rewriter inserted is a valid target for its own later `replace` or
/// `remove` in the same invocation. An edit the editor accepted that then
/// fails to materialise aborts the process (see the
/// [failure contract](self#failure-and-drop-contract)).
pub trait Rewriter: Send + Sync {
    /// Edit extension blocks through `editor` — insert/replace/remove only,
    /// never the primary, payload, or BIB/BCB blocks, and never a block under
    /// existing BPSec coverage. `bundle` and `reader` are the wire form as
    /// this invocation received it. `context` carries the boundary (and, at
    /// Egress, the resolved next hop). Return [`Verdict::Drop`] to abort the
    /// attempt.
    fn rewrite<'a>(
        &self,
        bundle: &Bundle,
        reader: &'a dyn Reader<'a>,
        metadata: &BundleMetadata,
        context: RewriteContext<'_>,
        editor: &mut ExtensionEditor<'_>,
    ) -> Verdict;
}

/// The boundary a [`Rewriter`] is invoked at, with any hook-specific context.
///
/// Next-hop context is Egress-only — a delivering bundle terminates here and
/// has no next hop — so it rides the variant rather than the method signature,
/// letting one trait serve both boundaries.
#[derive(Clone, Copy, Debug)]
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
