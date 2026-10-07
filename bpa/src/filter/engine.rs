//! The chain runner: executes the frozen per-hook chains inline.
//!
//! Filter invocations are synchronous and the reader borrows the caller's
//! decoded bundle, buffer, BCB OperationSets, and key source — borrows that
//! cannot cross a spawn boundary — so every chain runs inline on the calling
//! task. An empty chain costs one branch: nothing is parsed and nothing is
//! allocated. A Rewriter that edits costs a full copy of the wire form — the
//! edits are materialised before the next link reads them — so each editing
//! link adds one bundle's size to the attempt's peak memory, on every attempt.
//!
//! Every runner returns the bundle to the caller — with every verdict, and
//! on an input chain's error path — so a claimed bundle's status is always
//! resolved by the site that claimed it: no re-fetch, no restore path. The
//! output chains have no error path. Their bytes were validated at ingress,
//! so an output chain that cannot decode them has met a BPA bug or storage
//! corruption; and a Rewriter execution failure means a rewrite that was
//! meant to work has not. Either way processing beyond it is undefined, and
//! the engine panics, which aborts the process — the fail-fast rule, by
//! analogy with a storage fault.

use core::{fmt::Debug, mem::take};

use hardy_bpv7::{
    bpsec::{
        DecryptingReader, bcb,
        key::{KeySet, KeySource},
    },
    editor::Chunk,
    eid::Eid,
    extension_editor::ExtensionEditor,
    parse::{Parsed, parse},
    status_report::ReasonCode,
};
use tracing::{debug, error};

use super::{
    Boundary, ClassifyContext, RewriteContext, Verdict, VerifyContext,
    pack::chains::{FilterChains, InputChain, RewriterEntry, VerifierEntry},
};
use crate::{Bytes, Error, HashMap, bundle::Bundle, keys::KeyProvider};

// One spelling per hook, shared by the metric labels and diagnostics.
const INGRESS: &str = "ingress";
const ORIGINATE: &str = "originate";
const EGRESS: &str = "egress";
const DELIVER: &str = "deliver";

/// A hook chain's verdict over a bundle. An input chain's errors travel
/// separately — as `(Bundle, error)`, keeping the bundle with its claimant.
/// The large `Err` variant is deliberate: boxing the bundle to shrink it
/// would tax every call site (cf. `cla::peers::forward`).
pub enum ChainOutcome {
    /// The bundle passed the chain; the pair remains consistent (a Rewriter
    /// pass returns the rewritten bytes and re-indexed block map).
    Continue(Bundle, Bytes),
    /// A filter dropped the bundle, optionally with a status-report reason.
    Drop(Bundle, Option<ReasonCode>),
}

type RunResult = Result<ChainOutcome, (Bundle, Error)>;

/// The engine's one spelling of the Verifier pass, returning the first
/// Drop verdict's reason (`None` = every Verifier passed).
fn check_verifiers(
    verifiers: &[VerifierEntry],
    hook: &'static str,
    ctx: &VerifyContext<'_>,
) -> Option<Option<ReasonCode>> {
    for entry in verifiers {
        if let Verdict::Drop(reason) = entry.verifier.verify(ctx) {
            debug!("Verifier '{}' dropped bundle: {reason:?}", entry.label);
            metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
            return Some(reason);
        }
    }
    None
}

// A Rewriter execution failure: logged, then the panic that aborts the
// process. Cold, and the only place the message is formatted, so the
// success path pays for no diagnostics.
#[cold]
fn rewriter_failed(label: &str, what: &str, e: impl Debug) -> ! {
    error!("Rewriter '{label}' {what}: {e:?}");
    panic!("Rewriter '{label}' {what}: {e:?}")
}

// An output chain's bytes failing the engine's own decode: they were
// validated at ingress and come back from storage, so the failure is a BPA
// bug or storage corruption.
// Logged, then the panic that aborts the process.
#[cold]
fn output_undecodable(hook: &str, e: impl Debug) -> ! {
    error!("The {hook} chain's bytes do not decode: {e:?}");
    panic!("The {hook} chain's bytes do not decode: {e:?}")
}

// An output chain's state after its Rewriter stage: the bundle and the wire
// form the last link left, with the decode products and key source derived
// from that form, which the Deliver Verifiers read.
struct Rewritten {
    bundle: Bundle,
    buf: Bytes,
    bcbs: HashMap<u64, bcb::OperationSet>,
    keys: Box<dyn KeySource>,
}

impl FilterChains {
    /// Runs the Ingress chain (Verifiers, then Classifiers) on the resident
    /// buffer `data` and its already-decoded BCB OperationSets. At the
    /// streaming gate `data` is the header prefix — the payload is not yet
    /// resident, so a filter reading it gets the reader's `NotResident` —
    /// and the caller threads in the `bcbs` the gate's header pass decoded.
    #[allow(clippy::result_large_err)]
    pub(crate) fn run_ingress(
        &self,
        bundle: Bundle,
        data: Bytes,
        bcbs: &HashMap<u64, bcb::OperationSet>,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        if self.ingress.verifiers.is_empty() && self.ingress.classifiers.is_empty() {
            return Ok(ChainOutcome::Continue(bundle, data));
        }
        self.run_input_decoded(&self.ingress, INGRESS, bundle, data, bcbs, key_provider)
    }

    /// Whether the Ingress chain has any registered links. The streaming gate
    /// checks this to skip the pre-drain header re-decode and clone entirely
    /// when nothing would run.
    pub(crate) fn has_ingress(&self) -> bool {
        !self.ingress.verifiers.is_empty() || !self.ingress.classifiers.is_empty()
    }

    /// Runs the Originate chain: Verifiers, then Classifiers sequentially.
    #[allow(clippy::result_large_err)]
    pub(crate) fn run_originate(
        &self,
        bundle: Bundle,
        data: Bytes,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        self.run_input(&self.originate, ORIGINATE, bundle, data, key_provider)
    }

    /// Runs the Egress chain: Rewriters sequentially, each invocation's
    /// edits materialised into the wire form before the next reads it.
    /// Nothing at Egress drops a bundle, so the chain returns the rewritten
    /// pair.
    pub(crate) fn run_egress(
        &self,
        bundle: Bundle,
        data: Bytes,
        next_hop: &Eid,
        key_provider: &dyn KeyProvider,
    ) -> (Bundle, Bytes) {
        if self.egress.is_empty() {
            return (bundle, data);
        }
        let Rewritten { bundle, buf, .. } = rewrite(
            &self.egress,
            EGRESS,
            Boundary::Egress { next_hop },
            bundle,
            data,
            key_provider,
        );
        (bundle, buf)
    }

    /// Runs the Deliver chain: Rewriters sequentially, then Verifiers.
    pub(crate) fn run_deliver(
        &self,
        bundle: Bundle,
        data: Bytes,
        key_provider: &dyn KeyProvider,
    ) -> ChainOutcome {
        let chain = &self.deliver;
        if chain.rewriters.is_empty() && chain.verifiers.is_empty() {
            return ChainOutcome::Continue(bundle, data);
        }
        let Rewritten {
            bundle,
            buf,
            bcbs,
            keys,
        } = rewrite(
            &chain.rewriters,
            DELIVER,
            Boundary::Deliver,
            bundle,
            data,
            key_provider,
        );

        let reader = DecryptingReader::new(&bundle.bpv7.blocks, &buf, &bcbs, &*keys);
        if let Some(reason) = check_verifiers(
            &chain.verifiers,
            DELIVER,
            &VerifyContext::new(&bundle.bpv7, &reader, &bundle.metadata),
        ) {
            return ChainOutcome::Drop(bundle, reason);
        }

        ChainOutcome::Continue(bundle, buf)
    }

    #[allow(clippy::result_large_err)]
    fn run_input(
        &self,
        chain: &InputChain,
        hook: &'static str,
        bundle: Bundle,
        data: Bytes,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        if chain.verifiers.is_empty() && chain.classifiers.is_empty() {
            return Ok(ChainOutcome::Continue(bundle, data));
        }

        // One decode pass per hook crossing: the OperationSets and the
        // returned buffer feed every invocation of this pass.
        let (buf, bcbs) = match parse(data) {
            Ok(Parsed { data, bcbs, .. }) => (data, bcbs),
            Err(e) => {
                metrics::counter!("bpa.filter.error", "hook" => hook).increment(1);
                return Err((bundle, e.into()));
            }
        };
        self.run_input_decoded(chain, hook, bundle, buf, &bcbs, key_provider)
    }

    // The Verifier-then-Classifier pass over a resident buffer whose BCB
    // OperationSets are already decoded — both input doors thread in the set
    // from their one header decode (`buf` is the header prefix, and payload
    // reads return the reader's `NotResident`).
    #[allow(clippy::result_large_err)]
    fn run_input_decoded(
        &self,
        chain: &InputChain,
        hook: &'static str,
        mut bundle: Bundle,
        buf: Bytes,
        bcbs: &HashMap<u64, bcb::OperationSet>,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        // A BPSec-free bundle never consults keys (decrypted reads exist
        // only for blocks under a BCB), so skip the provider round-trip.
        let keys: Box<dyn KeySource> = if bcbs.is_empty() {
            Box::new(KeySet::EMPTY)
        } else {
            key_provider.key_source(&bundle.bpv7, &buf)
        };

        // The reader lends the *wire* view only, so one reader — and its
        // decrypt memo — serves the whole pass: the delta applications
        // below touch `bundle.metadata`, a disjoint borrow.
        let reader = DecryptingReader::new(&bundle.bpv7.blocks, &buf, bcbs, &*keys);

        if let Some(reason) = check_verifiers(
            &chain.verifiers,
            hook,
            &VerifyContext::new(&bundle.bpv7, &reader, &bundle.metadata),
        ) {
            return Ok(ChainOutcome::Drop(bundle, reason));
        }

        for entry in chain.classifiers.iter() {
            // Each delta is applied before the next link runs: a Classifier
            // sees the metadata its predecessors wrote.
            let ctx = ClassifyContext::new(&bundle.bpv7, &reader, &bundle.metadata);
            match entry.classifier.classify(&ctx) {
                Verdict::Continue(delta) => bundle.metadata.apply(delta),
                Verdict::Drop(reason) => {
                    debug!("Classifier '{}' dropped bundle: {reason:?}", entry.label);
                    metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
                    return Ok(ChainOutcome::Drop(bundle, reason));
                }
            }
        }

        Ok(ChainOutcome::Continue(bundle, buf))
    }
}

// The Rewriter stage both output chains share. The parse and key source are
// the loop's invariant: derived once before the first link, and re-derived
// only when an edit materialises new bytes.
fn rewrite(
    rewriters: &[RewriterEntry],
    hook: &'static str,
    boundary: Boundary<'_>,
    mut bundle: Bundle,
    data: Bytes,
    key_provider: &dyn KeyProvider,
) -> Rewritten {
    let Parsed {
        data: mut buf,
        mut bcbs,
        ..
    } = parse(data).unwrap_or_else(|e| output_undecodable(hook, e));
    let mut keys = key_provider.key_source(&bundle.bpv7, &buf);

    for entry in rewriters {
        // The context, its reader and its editor are rebuilt per link
        // because the wire view they lend is exactly what an accepted edit
        // replaces (buf, block map, keys). The editor's edits are
        // materialised before the context's borrows end. A Rewriter
        // execution failure aborts: like a storage fault, an edit that was
        // meant to work and has not leaves every subsequent processing step
        // undefined — there is no error a caller could react to
        // appropriately.
        let finished = {
            let reader = DecryptingReader::new(&bundle.bpv7.blocks, &buf, &bcbs, &*keys);
            let mut ctx = RewriteContext::new(
                &bundle.bpv7,
                &reader,
                &bundle.metadata,
                boundary,
                ExtensionEditor::new(&bundle.bpv7, &buf),
            );
            entry.rewriter.rewrite(&mut ctx);
            ctx.into_editor()
                .finish()
                .unwrap_or_else(|e| rewriter_failed(&entry.label, "failed", e))
        };
        if let Some((new_bundle, chunks)) = finished {
            // Keep the (bundle, data) pair consistent for the next link: the
            // rebuilt block map indexes the rewritten bytes, and keeps the
            // BPSec coverage stamps of the blocks it carried over. The
            // record's primary — and with it the bundle id every store
            // operation is keyed on — is never replaced.
            let flat = Chunk::flatten_bytes(chunks, take(&mut buf));
            let Parsed {
                data: new_buf,
                bcbs: new_bcbs,
                ..
            } = parse(flat).unwrap_or_else(|e| {
                rewriter_failed(&entry.label, "produced an unparseable bundle", e)
            });
            (buf, bcbs) = (new_buf, new_bcbs);
            bundle.bpv7.blocks = new_bundle.blocks;
            keys = key_provider.key_source(&bundle.bpv7, &buf);
            metrics::counter!("bpa.filter.modified", "hook" => hook).increment(1);
        }
    }

    Rewritten {
        bundle,
        buf,
        bcbs,
        keys,
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use alloc::borrow::Cow;

    use hardy_async::sync::spin::Mutex;
    // Aliased: the wire bundle, beside the record `Bundle` from `super`.
    use hardy_bpv7::{
        Bundle as Bpv7Bundle, block,
        builder::Builder,
        crc::CrcType,
        creation_timestamp::CreationTimestamp,
        extension_editor::Error,
        reader::{Availability, ReaderExt},
        status_report::ReasonCode,
    };
    use hardy_cbor::encode::emit;

    use super::*;
    use crate::{
        Arc,
        bundle::{BundleMetadata, BundleStatus},
        filter::{
            Classifier, Rewriter, Verifier,
            pack::{FilterPack, chains::FilterChains},
            slots::{MetadataDelta, Slot},
        },
        keys::NullKeyProvider,
    };

    fn test_bundle() -> (Bundle, Bytes, HashMap<u64, bcb::OperationSet>) {
        let (_, data) = Builder::new("ipn:1.1".parse().unwrap(), "ipn:99.1".parse().unwrap())
            .with_payload(Cow::Borrowed(b"engine-test"))
            .build(CreationTimestamp::now())
            .unwrap();
        // Parse so the bundle, its resident bytes, and the BCB OperationSets
        // all come from one decode pass — as they do at every real hook.
        let Parsed {
            bundle, data, bcbs, ..
        } = parse(Bytes::from(data)).unwrap();
        (
            Bundle {
                bpv7: bundle,
                metadata: BundleMetadata::originated(),
                status: BundleStatus::New,
            },
            data,
            bcbs,
        )
    }

    fn freeze(pack: FilterPack) -> FilterChains {
        FilterChains::freeze(vec![pack])
    }

    crate::slot!(static MARK: u32);

    struct SlotWriter(&'static Slot<u32>, u32);

    impl Classifier for SlotWriter {
        fn classify(&self, _ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
            let mut delta = MetadataDelta::default();
            delta.set(self.0, &self.1);
            Verdict::Continue(delta)
        }
    }

    // Drops unless the slot already carries the expected value — proves the
    // preceding link's delta was applied before this invocation.
    struct SlotExpecter(&'static Slot<u32>, u32);

    impl Classifier for SlotExpecter {
        fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
            if ctx.metadata().slot(self.0) == Some(self.1) {
                Verdict::Continue(MetadataDelta::default())
            } else {
                Verdict::Drop(Some(ReasonCode::NoAdditionalInformation))
            }
        }
    }

    #[test]
    fn classifier_sees_preceding_deltas_and_result_persists() {
        let mut pack = FilterPack::new("test");
        pack.ingress_classifier("writer", SlotWriter(&MARK, 7));
        pack.ingress_classifier("expecter", SlotExpecter(&MARK, 7));
        let chains = freeze(pack);

        let (bundle, data, bcbs) = test_bundle();
        let Ok(ChainOutcome::Continue(bundle, _)) =
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider)
        else {
            panic!("expecter must have seen the writer's delta");
        };
        assert_eq!(bundle.metadata.slot(&MARK), Some(7));
    }

    // Drops if the slot is set: proves no Classifier has run before it.
    struct UnclassifiedVerifier(&'static Slot<u32>);

    impl Verifier for UnclassifiedVerifier {
        fn verify(&self, ctx: &VerifyContext<'_>) -> Verdict {
            match ctx.metadata().slot(self.0) {
                None => Verdict::Continue(()),
                Some(_) => Verdict::Drop(None),
            }
        }
    }

    #[test]
    fn input_verifiers_run_before_classifiers() {
        let mut pack = FilterPack::new("test");
        // The Classifier is registered first: the stage order, not the call
        // order, keeps the Verifier ahead of it.
        pack.ingress_classifier("writer", SlotWriter(&MARK, 7));
        pack.ingress_verifier("unclassified", UnclassifiedVerifier(&MARK));
        let chains = freeze(pack);

        let (bundle, data, bcbs) = test_bundle();
        let Ok(ChainOutcome::Continue(bundle, _)) =
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider)
        else {
            panic!("the Verifier must run before the Classifier writes the slot");
        };
        assert_eq!(bundle.metadata.slot(&MARK), Some(7));
    }

    struct DropVerifier;

    impl Verifier for DropVerifier {
        fn verify(&self, _ctx: &VerifyContext<'_>) -> Verdict {
            Verdict::Drop(Some(ReasonCode::BlockUnintelligible))
        }
    }

    #[test]
    fn verifier_drop_carries_its_reason() {
        let mut pack = FilterPack::new("test");
        pack.ingress_verifier("dropper", DropVerifier);
        let chains = freeze(pack);

        let (bundle, data, bcbs) = test_bundle();
        let Ok(ChainOutcome::Drop(_, reason)) =
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider)
        else {
            panic!("verifier must drop the bundle");
        };
        assert_eq!(reason, Some(ReasonCode::BlockUnintelligible));
    }

    // Counts its invocations, so one shared instance can be seen serving
    // two hooks.
    struct CountingVerifier(AtomicUsize);

    impl Verifier for CountingVerifier {
        fn verify(&self, _ctx: &VerifyContext<'_>) -> Verdict {
            self.0.fetch_add(1, Ordering::Relaxed);
            Verdict::Continue(())
        }
    }

    #[test]
    fn boxed_and_shared_filters_register() {
        // A filter built from configuration arrives as a boxed trait object.
        let boxed: Box<dyn Verifier> = Box::new(DropVerifier);
        let mut pack = FilterPack::new("test");
        pack.ingress_verifier("boxed", boxed);
        let chains = freeze(pack);

        let (bundle, data, bcbs) = test_bundle();
        let Ok(ChainOutcome::Drop(_, reason)) =
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider)
        else {
            panic!("the boxed Verifier must run");
        };
        assert_eq!(reason, Some(ReasonCode::BlockUnintelligible));

        // One instance behind an `Arc` serves two hooks.
        let shared = Arc::new(CountingVerifier(AtomicUsize::new(0)));
        let mut pack = FilterPack::new("test");
        pack.ingress_verifier("shared", shared.clone());
        pack.deliver_verifier("shared", shared.clone());
        let chains = freeze(pack);

        let (bundle, data, bcbs) = test_bundle();
        let Ok(ChainOutcome::Continue(bundle, data)) =
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider)
        else {
            panic!("the shared Verifier passes at Ingress");
        };
        let ChainOutcome::Continue(..) = chains.run_deliver(bundle, data, &NullKeyProvider) else {
            panic!("the shared Verifier passes at Deliver");
        };
        assert_eq!(shared.0.load(Ordering::Relaxed), 2);
    }

    const CUSTOM_BLOCK: block::Type = block::Type::Unrecognised(192);

    struct BlockInserter;

    impl Rewriter for BlockInserter {
        fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
            if let Boundary::Egress { next_hop } = ctx.boundary() {
                assert_eq!(next_hop, &"ipn:2.0".parse::<Eid>().unwrap());
            }
            assert!(
                ctx.reader()
                    .block_header(1)
                    .is_some_and(|b| b.block_type == block::Type::Payload)
            );
            // A refusal is a Rewriter's no-match path, never a panic (a
            // panicking filter aborts the node); the tests below check the
            // insert landed.
            let _ = ctx.editor().insert(
                CUSTOM_BLOCK,
                block::Flags::default(),
                CrcType::None,
                emit(&42u64).0.into(),
            );
        }
    }

    // Gates on the predecessor's edit being visible with consistent extents:
    // the inserted block decodes from the rewritten bytes, and the payload
    // still reads back intact.
    struct BlockExpecter;

    impl Verifier for BlockExpecter {
        fn verify(&self, ctx: &VerifyContext<'_>) -> Verdict {
            let reader = ctx.reader();
            let Some((&number, _)) = ctx
                .bundle()
                .blocks
                .iter()
                .find(|(_, b)| b.block_type == CUSTOM_BLOCK)
            else {
                return Verdict::Drop(None);
            };
            if reader.extract::<u64>(number).ok().flatten() != Some(42) {
                return Verdict::Drop(None);
            }
            match reader.block(1) {
                Some((_, Availability::Available(payload)))
                    if payload.as_ref() == b"engine-test" =>
                {
                    Verdict::Continue(())
                }
                _ => Verdict::Drop(None),
            }
        }
    }

    #[test]
    fn rewriter_edit_reaches_the_deliver_verifier_consistently() {
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("inserter", BlockInserter);
        pack.deliver_verifier("expecter", BlockExpecter);
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
        let ChainOutcome::Continue(bundle, data) =
            chains.run_deliver(bundle, data, &NullKeyProvider)
        else {
            panic!("the verifier must have seen the inserted block");
        };

        // The returned pair reparses: the rewrite really is on the wire.
        let Parsed { bundle: raw, .. } = parse(data).unwrap();
        assert!(raw.blocks.values().any(|b| b.block_type == CUSTOM_BLOCK));
        assert!(
            bundle
                .bpv7
                .blocks
                .values()
                .any(|b| b.block_type == CUSTOM_BLOCK)
        );
    }

    // The Egress chain hands back the rewritten pair with no gate after it:
    // the returned block map must index the returned bytes.
    #[test]
    fn egress_returns_a_consistent_rewritten_pair() {
        let mut pack = FilterPack::new("test");
        pack.egress_rewriter("inserter", BlockInserter);
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        let (bundle, data) = chains.run_egress(bundle, data, &next_hop, &NullKeyProvider);

        let block = bundle
            .bpv7
            .blocks
            .values()
            .find(|b| b.block_type == CUSTOM_BLOCK)
            .expect("the insert is in the returned block map");
        let body = block
            .payload(&data)
            .expect("the inserted block is resident");
        assert_eq!(body, emit(&42u64).0.as_slice());
        let payload = bundle.bpv7.blocks[&1].payload(&data).unwrap();
        assert_eq!(payload, b"engine-test");
    }

    struct PayloadAttacker;

    impl Rewriter for PayloadAttacker {
        fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
            let editor = ctx.editor();
            assert!(matches!(
                editor.insert(
                    block::Type::BlockIntegrity,
                    block::Flags::default(),
                    CrcType::None,
                    Box::from(&[0u8][..]),
                ),
                Err(Error::ReservedType(_))
            ));
            assert!(matches!(editor.remove(1), Err(Error::ReservedBlock(1))));
            assert!(matches!(
                editor.replace(0, Box::from(&[0u8][..])),
                Err(Error::ReservedBlock(0))
            ));
            assert!(matches!(editor.remove(9), Err(Error::NoSuchBlock(9))));
        }
    }

    #[test]
    fn rewriter_editor_refuses_out_of_scope_edits() {
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("attacker", PayloadAttacker);
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
        let ChainOutcome::Continue(_, out) =
            chains.run_deliver(bundle, data.clone(), &NullKeyProvider)
        else {
            panic!("a Rewriter has no verdict, so the chain continues");
        };
        // Nothing was edited: the bytes pass through unchanged.
        assert_eq!(out, data);
    }

    // Counts key-source derivations: the output chain's parse and key
    // source are a loop invariant, so a chain of non-editing links derives
    // exactly once however many links run.
    struct CountingProvider(AtomicUsize);

    impl KeyProvider for CountingProvider {
        fn key_source(&self, _bundle: &Bpv7Bundle, _data: &[u8]) -> Box<dyn KeySource> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::new(KeySet::EMPTY)
        }
    }

    struct NoopRewriter;

    impl Rewriter for NoopRewriter {
        fn rewrite(&self, _ctx: &mut RewriteContext<'_>) {}
    }

    struct PassVerifier;

    impl Verifier for PassVerifier {
        fn verify(&self, _ctx: &VerifyContext<'_>) -> Verdict {
            Verdict::Continue(())
        }
    }

    #[test]
    fn output_chain_derives_keys_once_without_edits() {
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("noop-a", NoopRewriter);
        pack.deliver_rewriter("noop-b", NoopRewriter);
        pack.deliver_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
        let provider = CountingProvider(AtomicUsize::new(0));
        let ChainOutcome::Continue(..) = chains.run_deliver(bundle, data, &provider) else {
            panic!("a chain of passing links must continue");
        };
        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            1,
            "two non-editing Rewriters and a Verifier share one derivation"
        );
    }

    // Records the block count of every bundle it derives a key source for.
    struct BlockCountProvider(Mutex<Vec<usize>>);

    impl KeyProvider for BlockCountProvider {
        fn key_source(&self, bundle: &Bpv7Bundle, _data: &[u8]) -> Box<dyn KeySource> {
            self.0.lock().push(bundle.blocks.len());
            Box::new(KeySet::EMPTY)
        }
    }

    // The provider receives the block map, so the re-derivation after an
    // edit must see the rebuilt one: keys derived against the stale map
    // would serve the rest of the chain.
    #[test]
    fn keys_rederive_against_the_rewritten_block_map() {
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("inserter", BlockInserter);
        pack.deliver_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
        let before = bundle.bpv7.blocks.len();
        let provider = BlockCountProvider(Mutex::new(Vec::new()));
        let ChainOutcome::Continue(..) = chains.run_deliver(bundle, data, &provider) else {
            panic!("an inserting Rewriter and a passing Verifier must continue");
        };
        assert_eq!(*provider.0.lock(), [before, before + 1]);
    }

    // The bundle one byte short: it no longer decodes.
    fn truncated_bundle() -> (Bundle, Bytes) {
        let (bundle, data, _) = test_bundle();
        let data = data.slice(..data.len() - 1);
        (bundle, data)
    }

    // An output chain's bytes were validated at ingress, so failing its
    // decode is a BPA bug or storage corruption: fatal, never a park. The
    // panic is caught here because the chain runs outside a pipeline task.
    #[test]
    #[should_panic(expected = "The egress chain's bytes do not decode")]
    fn undecodable_egress_bytes_are_fatal() {
        let mut pack = FilterPack::new("test");
        pack.egress_rewriter("noop", NoopRewriter);
        let chains = freeze(pack);

        let (bundle, data) = truncated_bundle();
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        chains.run_egress(bundle, data, &next_hop, &NullKeyProvider);
    }

    #[test]
    #[should_panic(expected = "The deliver chain's bytes do not decode")]
    fn undecodable_deliver_bytes_are_fatal() {
        let mut pack = FilterPack::new("test");
        pack.deliver_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data) = truncated_bundle();
        chains.run_deliver(bundle, data, &NullKeyProvider);
    }

    #[test]
    fn bpsec_free_input_skips_key_derivation() {
        let mut pack = FilterPack::new("test");
        pack.ingress_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data, bcbs) = test_bundle();
        assert!(bcbs.is_empty(), "the fixture bundle carries no BCB");
        let provider = CountingProvider(AtomicUsize::new(0));
        let Ok(ChainOutcome::Continue(..)) = chains.run_ingress(bundle, data, &bcbs, &provider)
        else {
            panic!("a passing Verifier must continue");
        };
        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            0,
            "a BPSec-free bundle never consults the key provider"
        );
    }
}
