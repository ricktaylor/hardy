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
//! Every runner returns the bundle to the caller on both the verdict and the
//! error path, so a claimed bundle's status is always resolved by the site
//! that claimed it — no re-fetch, no restore path. A Rewriter execution
//! failure is no error at all: the rewrite was meant to work and has not,
//! so processing beyond it is undefined and the engine panics naming the
//! link, which aborts the process — the fail-fast rule, by analogy with a
//! storage fault.

use core::{fmt::Debug, mem::take};

use hardy_bpv7::{
    bpsec::{
        DecryptingReader,
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
    pack::chains::{FilterChains, InputChain, OutputChain, VerifierEntry},
};
use crate::{Bytes, Error, bundle::Bundle, keys::KeyProvider};

// One spelling per hook, shared by the metric labels and diagnostics.
const INGRESS: &str = "ingress";
const ORIGINATE: &str = "originate";
const EGRESS: &str = "egress";
const DELIVER: &str = "deliver";

/// A hook chain's verdict over a bundle. Errors travel separately — as
/// `(Bundle, error)`, keeping the bundle with its claimant. The large `Err`
/// variant is deliberate: boxing the bundle to shrink it would tax every
/// call site (cf. `cla::peers::forward`).
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

impl FilterChains {
    /// Runs the Ingress chain: Verifiers, then Classifiers sequentially.
    #[allow(clippy::result_large_err)]
    pub(crate) fn run_ingress(
        &self,
        bundle: Bundle,
        data: Bytes,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        self.run_input(&self.ingress, INGRESS, bundle, data, key_provider)
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

    /// Runs the Egress chain: Rewriters sequentially — each invocation's
    /// edits are materialised into the wire form before the next reads it —
    /// then Verifiers gating the final pre-BPSec form.
    #[allow(clippy::result_large_err)]
    pub(crate) fn run_egress(
        &self,
        bundle: Bundle,
        data: Bytes,
        next_hop: &Eid,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        self.run_output(
            &self.egress,
            EGRESS,
            Boundary::Egress { next_hop },
            bundle,
            data,
            key_provider,
        )
    }

    /// Runs the Deliver chain: Rewriters sequentially, then Verifiers.
    #[allow(clippy::result_large_err)]
    pub(crate) fn run_deliver(
        &self,
        bundle: Bundle,
        data: Bytes,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        self.run_output(
            &self.deliver,
            DELIVER,
            Boundary::Deliver,
            bundle,
            data,
            key_provider,
        )
    }

    #[allow(clippy::result_large_err)]
    fn run_input(
        &self,
        chain: &InputChain,
        hook: &'static str,
        mut bundle: Bundle,
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
        let reader = DecryptingReader::new(&bundle.bpv7.blocks, &buf, &bcbs, &*keys);

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

    #[allow(clippy::result_large_err)]
    fn run_output(
        &self,
        chain: &OutputChain,
        hook: &'static str,
        boundary: Boundary<'_>,
        mut bundle: Bundle,
        data: Bytes,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        if chain.rewriters.is_empty() && chain.verifiers.is_empty() {
            return Ok(ChainOutcome::Continue(bundle, data));
        }

        // The parse and key source are the loop's invariant: derived once
        // before the first link, re-derived only when an edit materialises
        // new bytes, and read as-is by the trailing Verifier stage.
        let (mut buf, mut bcbs) = match parse(data) {
            Ok(Parsed { data, bcbs, .. }) => (data, bcbs),
            Err(e) => {
                metrics::counter!("bpa.filter.error", "hook" => hook).increment(1);
                return Err((bundle, e.into()));
            }
        };
        let mut keys = key_provider.key_source(&bundle.bpv7, &buf);

        for entry in chain.rewriters.iter() {
            // The context, its reader and its editor are rebuilt per link
            // because the wire view they lend is exactly what an accepted
            // edit replaces (buf, block map, keys). The editor's edits are
            // materialised before the context's borrows end.
            let verdict = {
                let reader = DecryptingReader::new(&bundle.bpv7.blocks, &buf, &bcbs, &*keys);
                let mut ctx = RewriteContext::new(
                    &bundle.bpv7,
                    &reader,
                    &bundle.metadata,
                    boundary,
                    ExtensionEditor::new(&bundle.bpv7, &buf),
                );
                match entry.rewriter.rewrite(&mut ctx) {
                    Verdict::Drop(reason) => Verdict::Drop(reason),
                    // A Rewriter execution failure aborts: like a storage
                    // fault, an edit that was meant to work and has not
                    // leaves every subsequent processing step undefined —
                    // there is no error a caller could react to
                    // appropriately.
                    Verdict::Continue(()) => Verdict::Continue(
                        ctx.into_editor()
                            .finish()
                            .unwrap_or_else(|e| rewriter_failed(&entry.label, "failed", e)),
                    ),
                }
            };
            match verdict {
                Verdict::Drop(reason) => {
                    debug!("Rewriter '{}' dropped bundle: {reason:?}", entry.label);
                    metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
                    return Ok(ChainOutcome::Drop(bundle, reason));
                }
                Verdict::Continue(finished) => {
                    match finished {
                        None => {}
                        Some((new_bundle, chunks)) => {
                            // Keep the (bundle, data) pair consistent for
                            // the next link: the rebuilt block map indexes
                            // the rewritten bytes, and keeps the BPSec
                            // coverage stamps of the blocks it carried over.
                            // The record's primary — and with it the bundle
                            // id every store operation is keyed on — is
                            // never replaced.
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
                }
            }
        }

        let reader = DecryptingReader::new(&bundle.bpv7.blocks, &buf, &bcbs, &*keys);
        if let Some(reason) = check_verifiers(
            &chain.verifiers,
            hook,
            &VerifyContext::new(&bundle.bpv7, &reader, &bundle.metadata),
        ) {
            return Ok(ChainOutcome::Drop(bundle, reason));
        }

        Ok(ChainOutcome::Continue(bundle, buf))
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

    fn test_bundle() -> (Bundle, Bytes) {
        let (bundle, data) = Builder::new("ipn:1.1".parse().unwrap(), "ipn:99.1".parse().unwrap())
            .with_payload(Cow::Borrowed(b"engine-test"))
            .build(CreationTimestamp::now())
            .unwrap();
        (
            Bundle {
                bpv7: bundle,
                metadata: BundleMetadata::originated(),
                status: BundleStatus::New,
            },
            Bytes::from(data),
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

        let (bundle, data) = test_bundle();
        let Ok(ChainOutcome::Continue(bundle, _)) =
            chains.run_ingress(bundle, data, &NullKeyProvider)
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

        let (bundle, data) = test_bundle();
        let Ok(ChainOutcome::Continue(bundle, _)) =
            chains.run_ingress(bundle, data, &NullKeyProvider)
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

        let (bundle, data) = test_bundle();
        let Ok(ChainOutcome::Drop(_, reason)) = chains.run_ingress(bundle, data, &NullKeyProvider)
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

        let (bundle, data) = test_bundle();
        let Ok(ChainOutcome::Drop(_, reason)) = chains.run_ingress(bundle, data, &NullKeyProvider)
        else {
            panic!("the boxed Verifier must run");
        };
        assert_eq!(reason, Some(ReasonCode::BlockUnintelligible));

        // One instance behind an `Arc` serves two hooks.
        let shared = Arc::new(CountingVerifier(AtomicUsize::new(0)));
        let mut pack = FilterPack::new("test");
        pack.ingress_verifier("shared", shared.clone());
        pack.egress_verifier("shared", shared.clone());
        let chains = freeze(pack);

        let (bundle, data) = test_bundle();
        let Ok(ChainOutcome::Continue(bundle, data)) =
            chains.run_ingress(bundle, data, &NullKeyProvider)
        else {
            panic!("the shared Verifier passes at Ingress");
        };
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        let Ok(ChainOutcome::Continue(..)) =
            chains.run_egress(bundle, data, &next_hop, &NullKeyProvider)
        else {
            panic!("the shared Verifier passes at Egress");
        };
        assert_eq!(shared.0.load(Ordering::Relaxed), 2);
    }

    const CUSTOM_BLOCK: block::Type = block::Type::Unrecognised(192);

    struct BlockInserter;

    impl Rewriter for BlockInserter {
        fn rewrite(&self, ctx: &mut RewriteContext<'_>) -> Verdict {
            assert!(matches!(
                ctx.boundary(),
                Boundary::Egress { next_hop } if next_hop == &"ipn:2.0".parse().unwrap()
            ));
            assert!(
                ctx.reader()
                    .block_header(1)
                    .is_some_and(|b| b.block_type == block::Type::Payload)
            );
            // A refusal is a Rewriter's no-match path, never a panic (a
            // panicking filter aborts the node); the gating Verifier below
            // drops the bundle if this insert did not land.
            let _ = ctx.editor().insert(
                CUSTOM_BLOCK,
                block::Flags::default(),
                CrcType::None,
                emit(&42u64).0.into(),
            );
            Verdict::Continue(())
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
    fn rewriter_edit_reaches_the_gating_verifier_consistently() {
        let mut pack = FilterPack::new("test");
        pack.egress_rewriter("inserter", BlockInserter);
        pack.egress_verifier("expecter", BlockExpecter);
        let chains = freeze(pack);

        let (bundle, data) = test_bundle();
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        let Ok(ChainOutcome::Continue(bundle, data)) =
            chains.run_egress(bundle, data, &next_hop, &NullKeyProvider)
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

    struct PayloadAttacker;

    impl Rewriter for PayloadAttacker {
        fn rewrite(&self, ctx: &mut RewriteContext<'_>) -> Verdict {
            let editor = ctx.editor();
            let Err(Error::ReservedType(_)) = editor.insert(
                block::Type::BlockIntegrity,
                block::Flags::default(),
                CrcType::None,
                Box::from(&[0u8][..]),
            ) else {
                return Verdict::Drop(None);
            };
            let Err(Error::ReservedBlock(1)) = editor.remove(1) else {
                return Verdict::Drop(None);
            };
            let Err(Error::ReservedBlock(0)) = editor.replace(0, Box::from(&[0u8][..])) else {
                return Verdict::Drop(None);
            };
            let Err(Error::NoSuchBlock(9)) = editor.remove(9) else {
                return Verdict::Drop(None);
            };
            Verdict::Continue(())
        }
    }

    #[test]
    fn rewriter_editor_refuses_out_of_scope_edits() {
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("attacker", PayloadAttacker);
        let chains = freeze(pack);

        let (bundle, data) = test_bundle();
        let Ok(ChainOutcome::Continue(_, out)) =
            chains.run_deliver(bundle, data.clone(), &NullKeyProvider)
        else {
            panic!("every out-of-scope edit must be refused, not applied");
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
        fn rewrite(&self, _ctx: &mut RewriteContext<'_>) -> Verdict {
            Verdict::Continue(())
        }
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
        pack.egress_rewriter("noop-a", NoopRewriter);
        pack.egress_rewriter("noop-b", NoopRewriter);
        pack.egress_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data) = test_bundle();
        let provider = CountingProvider(AtomicUsize::new(0));
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        let Ok(ChainOutcome::Continue(..)) = chains.run_egress(bundle, data, &next_hop, &provider)
        else {
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
        pack.egress_rewriter("inserter", BlockInserter);
        pack.egress_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data) = test_bundle();
        let before = bundle.bpv7.blocks.len();
        let provider = BlockCountProvider(Mutex::new(Vec::new()));
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        let Ok(ChainOutcome::Continue(..)) = chains.run_egress(bundle, data, &next_hop, &provider)
        else {
            panic!("an inserting Rewriter and a passing Verifier must continue");
        };
        assert_eq!(*provider.0.lock(), [before, before + 1]);
    }

    #[test]
    fn bpsec_free_input_skips_key_derivation() {
        let mut pack = FilterPack::new("test");
        pack.ingress_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data) = test_bundle();
        let provider = CountingProvider(AtomicUsize::new(0));
        let Ok(ChainOutcome::Continue(..)) = chains.run_ingress(bundle, data, &provider) else {
            panic!("a passing Verifier must continue");
        };
        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            0,
            "a BPSec-free bundle never consults the key provider"
        );
    }
}
