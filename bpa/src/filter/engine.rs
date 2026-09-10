//! The chain runner: executes the frozen per-hook chains inline.
//!
//! Filter invocations are synchronous and the reader borrows the caller's
//! decoded bundle, buffer, BCB OperationSets, and key source — borrows that
//! cannot cross a spawn boundary — so every chain runs inline on the calling
//! task. An empty chain costs one branch: nothing is parsed and nothing is
//! allocated.
//!
//! Every runner returns the bundle to the caller on both the verdict and the
//! error path, so a claimed bundle's status is always resolved by the site
//! that claimed it — no re-fetch, no restore path. A Rewriter execution
//! failure is no error at all: the rewrite was meant to work and has not,
//! so processing beyond it is undefined and the engine fail-stops naming
//! the link (the storage-fault rule).

use core::mem::take;

use hardy_bpv7::{
    bpsec::{
        bcb,
        key::{KeySet, KeySource},
    },
    editor::Chunk,
    eid::Eid,
    parse::{Parsed, parse},
    status_report::ReasonCode,
};
use trace_err::*;
use tracing::debug;

use super::{
    BundleReader, RewriteContext, Verdict,
    editor::ScopedEditor,
    pack::chains::{FilterChains, InputChain, OutputChain, VerifierEntry},
};
use crate::{
    Bytes, HashMap,
    bundle::{Bundle, BundleMetadata},
    keys::KeyProvider,
};

// One spelling per hook, shared by the metric labels and diagnostics.
const INGRESS: &str = "ingress";
const ORIGINATE: &str = "originate";
const EGRESS: &str = "egress";
const DELIVER: &str = "deliver";

/// A hook chain's verdict over a bundle. Errors travel separately — as
/// `(Bundle, error)`, keeping the bundle with its claimant. The large `Err`
/// variant is deliberate: boxing the bundle to shrink it would tax every
/// call site (cf. `cla::peers::forward`).
pub(crate) enum ChainOutcome {
    /// The bundle passed the chain; the pair remains consistent (a Rewriter
    /// pass returns the rewritten bytes and re-indexed block map).
    Continue(Bundle, Bytes),
    /// A filter dropped the bundle, optionally with a status-report reason.
    Drop(Bundle, Option<ReasonCode>),
}

type RunResult = Result<ChainOutcome, (Bundle, crate::Error)>;

/// The engine's one spelling of the Verifier pass, returning the first
/// Drop verdict's reason (`None` = every Verifier passed).
fn check_verifiers(
    verifiers: &[VerifierEntry],
    hook: &'static str,
    reader: &BundleReader<'_>,
    metadata: &BundleMetadata,
) -> Option<Option<ReasonCode>> {
    for entry in verifiers {
        if let Verdict::Drop(reason) = entry.verifier.check(reader, metadata) {
            debug!("Verifier '{}' dropped bundle: {reason:?}", entry.label);
            metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
            return Some(reason);
        }
    }
    None
}

impl FilterChains {
    /// Runs the Ingress chain (Verifiers, then Classifiers) on the resident
    /// buffer `data` and its already-decoded BCB OperationSets. At the
    /// streaming gate `data` is the header prefix — the payload is not yet
    /// resident, so a filter reading it gets the reader's not-resident `None` —
    /// and the caller threads in `bcbs` re-derived from that prefix.
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

    /// Runs the Originate chain (Verifiers, then Classifiers) on the
    /// resident buffer `data` and its already-decoded BCB OperationSets —
    /// the header prefix at the originate door's admission stage, exactly
    /// as [`run_ingress`](Self::run_ingress) at the CLA gate.
    #[allow(clippy::result_large_err)]
    pub(crate) fn run_originate(
        &self,
        bundle: Bundle,
        data: Bytes,
        bcbs: &HashMap<u64, bcb::OperationSet>,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        if self.originate.verifiers.is_empty() && self.originate.classifiers.is_empty() {
            return Ok(ChainOutcome::Continue(bundle, data));
        }
        self.run_input_decoded(&self.originate, ORIGINATE, bundle, data, bcbs, key_provider)
    }

    /// Whether the Originate chain has any registered links, for the
    /// originate door's short-circuit — the twin of
    /// [`has_ingress`](Self::has_ingress).
    pub(crate) fn has_originate(&self) -> bool {
        !self.originate.verifiers.is_empty() || !self.originate.classifiers.is_empty()
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
            RewriteContext::Egress { next_hop },
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
            RewriteContext::Deliver,
            bundle,
            data,
            key_provider,
        )
    }

    // The Verifier-then-Classifier pass over a resident buffer whose BCB
    // OperationSets are already decoded — both input doors thread in the set
    // from their one header decode (`buf` is the header prefix, and payload
    // reads return the reader's not-resident `None`).
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

        // The reader lends the *wire* view only, so it is the whole pass's
        // invariant: the delta applications below touch `bundle.metadata`,
        // a disjoint borrow.
        let reader = BundleReader::new(&bundle.bpv7, &buf, bcbs, &*keys);

        if let Some(reason) = check_verifiers(&chain.verifiers, hook, &reader, &bundle.metadata) {
            return Ok(ChainOutcome::Drop(bundle, reason));
        }

        for entry in chain.classifiers.iter() {
            // Each delta is applied before the next link runs: a Classifier
            // sees the metadata its predecessors wrote.
            match entry.classifier.classify(&reader, &bundle.metadata) {
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
        context: RewriteContext<'_>,
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
            let mut editor = ScopedEditor::new(&bundle, &buf);
            // Rebuilt per link because the wire view it lends is exactly
            // what an accepted edit replaces (buf, block map, keys).
            let verdict = {
                let reader = BundleReader::new(&bundle.bpv7, &buf, &bcbs, &*keys);
                entry
                    .rewriter
                    .rewrite(&reader, &bundle.metadata, context, &mut editor)
            };
            match verdict {
                Verdict::Drop(reason) => {
                    debug!("Rewriter '{}' dropped bundle: {reason:?}", entry.label);
                    metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
                    return Ok(ChainOutcome::Drop(bundle, reason));
                }
                Verdict::Continue(()) => {
                    // A Rewriter execution failure fail-stops: like a
                    // storage fault, an edit that was meant to work and
                    // has not leaves every subsequent processing step
                    // undefined — there is no error a caller could react
                    // to appropriately.
                    match editor
                        .finish()
                        .trace_expect(&format!("Rewriter '{}' failed", entry.label))
                    {
                        None => {}
                        Some((new_bundle, chunks)) => {
                            // Keep the (bundle, data) pair consistent for
                            // the next link: the rebuilt block map indexes
                            // the rewritten bytes. The record's primary —
                            // and with it the bundle id every store
                            // operation is keyed on — is never replaced.
                            let flat = Chunk::flatten_bytes(chunks, take(&mut buf));
                            let Parsed {
                                data: new_buf,
                                bcbs: new_bcbs,
                                ..
                            } = parse(flat).trace_expect(&format!(
                                "Rewriter '{}' produced an unparseable bundle",
                                entry.label
                            ));
                            (buf, bcbs) = (new_buf, new_bcbs);
                            keys = key_provider.key_source(&bundle.bpv7, &buf);
                            bundle.bpv7.blocks = new_bundle.blocks;
                            metrics::counter!("bpa.filter.modified", "hook" => hook).increment(1);
                        }
                    }
                }
            }
        }

        let reader = BundleReader::new(&bundle.bpv7, &buf, &bcbs, &*keys);
        if let Some(reason) = check_verifiers(&chain.verifiers, hook, &reader, &bundle.metadata) {
            return Ok(ChainOutcome::Drop(bundle, reason));
        }

        Ok(ChainOutcome::Continue(bundle, buf))
    }
}

#[cfg(test)]
mod tests {
    use core::{
        num::NonZeroUsize,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use alloc::borrow::Cow;

    use hardy_bpv7::{
        block, builder::Builder, crc::CrcType, creation_timestamp::CreationTimestamp,
        status_report::ReasonCode,
    };
    use hardy_cbor::encode::emit;

    use super::*;
    use crate::{
        bundle::{BundleMetadata, BundleStatus},
        filter::{
            Classifier, Rewriter, Verifier,
            pack::{FilterPack, chains::FilterChains},
            slots::{MetadataDelta, SlotHandle},
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
        FilterChains::freeze(vec![pack]).unwrap().0
    }

    struct SlotWriter(SlotHandle<u32>, u32);

    impl Classifier for SlotWriter {
        fn classify(
            &self,
            _reader: &BundleReader<'_>,
            _metadata: &BundleMetadata,
        ) -> Verdict<MetadataDelta> {
            let mut delta = MetadataDelta::default();
            delta.set(&self.0, &self.1);
            Verdict::Continue(delta)
        }
    }

    // Drops unless the slot already carries the expected value — proves the
    // preceding link's delta was applied before this invocation.
    struct SlotExpecter(SlotHandle<u32>, u32);

    impl Classifier for SlotExpecter {
        fn classify(
            &self,
            _reader: &BundleReader<'_>,
            metadata: &BundleMetadata,
        ) -> Verdict<MetadataDelta> {
            if metadata.slot(&self.0) == Some(self.1) {
                Verdict::Continue(MetadataDelta::default())
            } else {
                Verdict::Drop(Some(ReasonCode::NoAdditionalInformation))
            }
        }
    }

    #[test]
    fn classifier_sees_preceding_deltas_and_result_persists() {
        let mut pack = FilterPack::new("test");
        let slot = pack.annotation_slot::<u32>("mark", NonZeroUsize::new(16).unwrap());
        pack.ingress_classifier("writer", SlotWriter(slot.clone(), 7));
        pack.ingress_classifier("expecter", SlotExpecter(slot.clone(), 7));
        let chains = freeze(pack);

        let (bundle, data, bcbs) = test_bundle();
        let Ok(ChainOutcome::Continue(bundle, _)) =
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider)
        else {
            panic!("expecter must have seen the writer's delta");
        };
        assert_eq!(bundle.metadata.slot(&slot), Some(7));
    }

    struct DropVerifier;

    impl Verifier for DropVerifier {
        fn check(&self, _reader: &BundleReader<'_>, _metadata: &BundleMetadata) -> Verdict {
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

    const CUSTOM_BLOCK: block::Type = block::Type::Unrecognised(192);

    struct BlockInserter;

    impl Rewriter for BlockInserter {
        fn rewrite(
            &self,
            reader: &BundleReader<'_>,
            _metadata: &BundleMetadata,
            context: RewriteContext<'_>,
            editor: &mut ScopedEditor<'_>,
        ) -> Verdict {
            assert!(matches!(
                context,
                RewriteContext::Egress { next_hop } if next_hop == &"ipn:2.0".parse().unwrap()
            ));
            assert!(
                reader
                    .block(1)
                    .is_some_and(|b| b.block_type == block::Type::Payload)
            );
            editor
                .insert(
                    CUSTOM_BLOCK,
                    block::Flags::default(),
                    CrcType::None,
                    emit(&42u64).0.into(),
                )
                .expect("insert of an extension block must be permitted");
            Verdict::Continue(())
        }
    }

    // Gates on the predecessor's edit being visible with consistent extents:
    // the inserted block decodes from the rewritten bytes, and the payload
    // still reads back intact.
    struct BlockExpecter;

    impl Verifier for BlockExpecter {
        fn check(&self, reader: &BundleReader<'_>, _metadata: &BundleMetadata) -> Verdict {
            let Some(number) = (2u64..16).find(|n| {
                reader
                    .block(*n)
                    .is_some_and(|b| b.block_type == CUSTOM_BLOCK)
            }) else {
                return Verdict::Drop(None);
            };
            if reader.extract::<u64>(number).ok().flatten() != Some(42) {
                return Verdict::Drop(None);
            }
            match reader.block_data(1) {
                Ok(Some(payload)) if payload.as_ref() == b"engine-test" => Verdict::Continue(()),
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

        let (bundle, data, _) = test_bundle();
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
        fn rewrite(
            &self,
            _reader: &BundleReader<'_>,
            _metadata: &BundleMetadata,
            _context: RewriteContext<'_>,
            editor: &mut ScopedEditor<'_>,
        ) -> Verdict {
            use crate::filter::editor::Error;

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
    fn scoped_editor_refuses_out_of_scope_edits() {
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("attacker", PayloadAttacker);
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
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
        fn key_source(&self, _bundle: &hardy_bpv7::Bundle, _data: &[u8]) -> Box<dyn KeySource> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::new(KeySet::EMPTY)
        }
    }

    struct NoopRewriter;

    impl Rewriter for NoopRewriter {
        fn rewrite(
            &self,
            _reader: &BundleReader<'_>,
            _metadata: &BundleMetadata,
            _context: RewriteContext<'_>,
            _editor: &mut ScopedEditor<'_>,
        ) -> Verdict {
            Verdict::Continue(())
        }
    }

    struct PassVerifier;

    impl Verifier for PassVerifier {
        fn check(&self, _reader: &BundleReader<'_>, _metadata: &BundleMetadata) -> Verdict {
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

        let (bundle, data, _) = test_bundle();
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
