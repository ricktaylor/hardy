//! The chain runner: executes the frozen per-hook chains inline.
//!
//! Filter invocations are synchronous and the reader borrows the caller's
//! decoded bundle, buffer, BCB OperationSets, and key source — borrows that
//! cannot cross a spawn boundary — so every chain runs inline on the calling
//! task. "Parallel" Verifiers is an independence contract (no ordering, no
//! cross-talk), not a spawning strategy. An empty chain costs one branch:
//! nothing is parsed and nothing is allocated.
//!
//! Every runner returns the bundle to the caller on both the verdict and the
//! error path, so a claimed bundle's status is always resolved by the site
//! that claimed it — no re-fetch, no restore path.

use hardy_bpv7::{bpsec::bcb, editor::Chunk, eid::Eid, parse::Parsed, status_report::ReasonCode};
use tracing::{debug, error};

use super::{
    BundleReader, RewriteContext, Verdict,
    editor::ScopedEditor,
    pack::chains::{FilterChains, InputChain, OutputChain},
};
use crate::{Bytes, HashMap, bundle::Bundle, keys::KeyProvider};

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

type RunResult = core::result::Result<ChainOutcome, (Bundle, crate::Error)>;

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
        self.run_input_decoded(&self.ingress, "ingress", bundle, data, bcbs, key_provider)
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
        self.run_input(&self.originate, "originate", bundle, data, key_provider)
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
            "egress",
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
            "deliver",
            RewriteContext::Deliver,
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
        bundle: Bundle,
        data: Bytes,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        if chain.verifiers.is_empty() && chain.classifiers.is_empty() {
            return Ok(ChainOutcome::Continue(bundle, data));
        }

        // One decode pass per hook crossing: the OperationSets and the
        // returned buffer feed every invocation of this pass.
        let (buf, bcbs) = match hardy_bpv7::parse::parse(data) {
            Ok(Parsed { data, bcbs, .. }) => (data, bcbs),
            Err(e) => {
                metrics::counter!("bpa.filter.error", "hook" => hook).increment(1);
                return Err((bundle, e.into()));
            }
        };
        self.run_input_decoded(chain, hook, bundle, buf, &bcbs, key_provider)
    }

    // The Verifier-then-Classifier pass over a resident buffer whose BCB
    // OperationSets are already decoded. `run_input` decodes them from the whole
    // bundle; the Ingress gate threads in the set re-derived from the header
    // prefix (`buf` is then that prefix, and payload reads return the reader's
    // not-resident `None`).
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
        let keys = key_provider.key_source(&bundle.bpv7, &buf);

        for entry in chain.verifiers.iter() {
            let reader = BundleReader::new(&bundle, &buf, bcbs, &*keys);
            if let Verdict::Drop(reason) = entry.verifier.check(&reader) {
                debug!("Verifier '{}' dropped bundle: {reason:?}", entry.label);
                metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
                return Ok(ChainOutcome::Drop(bundle, reason));
            }
        }

        for entry in chain.classifiers.iter() {
            // The reader's borrow ends before the delta is applied: a
            // Classifier sees the deltas applied by preceding links.
            let verdict = {
                let reader = BundleReader::new(&bundle, &buf, bcbs, &*keys);
                entry.classifier.classify(&reader)
            };
            match verdict {
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
        mut data: Bytes,
        key_provider: &dyn KeyProvider,
    ) -> RunResult {
        if chain.rewriters.is_empty() && chain.verifiers.is_empty() {
            return Ok(ChainOutcome::Continue(bundle, data));
        }

        for entry in chain.rewriters.iter() {
            let (buf, bcbs) = match hardy_bpv7::parse::parse(data) {
                Ok(Parsed { data, bcbs, .. }) => (data, bcbs),
                Err(e) => {
                    metrics::counter!("bpa.filter.error", "hook" => hook).increment(1);
                    return Err((bundle, e.into()));
                }
            };
            let keys = key_provider.key_source(&bundle.bpv7, &buf);

            let mut editor = ScopedEditor::new(&bundle, &buf);
            let verdict = {
                let reader = BundleReader::new(&bundle, &buf, &bcbs, &*keys);
                entry.rewriter.rewrite(&reader, context, &mut editor)
            };
            match verdict {
                Verdict::Drop(reason) => {
                    debug!("Rewriter '{}' dropped bundle: {reason:?}", entry.label);
                    metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
                    return Ok(ChainOutcome::Drop(bundle, reason));
                }
                Verdict::Continue(()) => match editor.finish() {
                    Ok(None) => data = buf,
                    Ok(Some((new_bundle, chunks))) => {
                        // Keep the (bundle, data) pair consistent for the
                        // next link: the rebuilt block map indexes the
                        // rewritten bytes. The record's primary — and with
                        // it the bundle id every store operation is keyed
                        // on — is never replaced.
                        data = Chunk::flatten_bytes(chunks, buf);
                        bundle.bpv7.blocks = new_bundle.blocks;
                        metrics::counter!("bpa.filter.modified", "hook" => hook).increment(1);
                    }
                    Err(e) => {
                        error!("Rewriter '{}' produced an invalid edit: {e}", entry.label);
                        metrics::counter!("bpa.filter.error", "hook" => hook).increment(1);
                        return Err((bundle, e.into()));
                    }
                },
            }
        }

        if chain.verifiers.is_empty() {
            return Ok(ChainOutcome::Continue(bundle, data));
        }

        let (buf, bcbs) = match hardy_bpv7::parse::parse(data) {
            Ok(Parsed { data, bcbs, .. }) => (data, bcbs),
            Err(e) => {
                metrics::counter!("bpa.filter.error", "hook" => hook).increment(1);
                return Err((bundle, e.into()));
            }
        };
        let keys = key_provider.key_source(&bundle.bpv7, &buf);

        for entry in chain.verifiers.iter() {
            let reader = BundleReader::new(&bundle, &buf, &bcbs, &*keys);
            if let Verdict::Drop(reason) = entry.verifier.check(&reader) {
                debug!("Verifier '{}' dropped bundle: {reason:?}", entry.label);
                metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
                return Ok(ChainOutcome::Drop(bundle, reason));
            }
        }

        Ok(ChainOutcome::Continue(bundle, buf))
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;

    use hardy_bpv7::{block, crc::CrcType, status_report::ReasonCode};

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
        let (_, data) = hardy_bpv7::builder::Builder::new(
            "ipn:1.1".parse().unwrap(),
            "ipn:99.1".parse().unwrap(),
        )
        .with_payload(alloc::borrow::Cow::Borrowed(b"engine-test"))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();
        // Parse so the bundle, its resident bytes, and the BCB OperationSets
        // all come from one decode pass — as they do at every real hook.
        let Parsed {
            bundle, data, bcbs, ..
        } = hardy_bpv7::parse::parse(Bytes::from(data)).unwrap();
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
        fn classify(&self, _reader: &BundleReader<'_>) -> Verdict<MetadataDelta> {
            let mut delta = MetadataDelta::default();
            delta.set(&self.0, &self.1);
            Verdict::Continue(delta)
        }
    }

    // Drops unless the slot already carries the expected value — proves the
    // preceding link's delta was applied before this invocation.
    struct SlotExpecter(SlotHandle<u32>, u32);

    impl Classifier for SlotExpecter {
        fn classify(&self, reader: &BundleReader<'_>) -> Verdict<MetadataDelta> {
            if reader.metadata().slot(&self.0) == Some(self.1) {
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
        fn check(&self, _reader: &BundleReader<'_>) -> Verdict {
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
                    hardy_cbor::encode::emit(&42u64).0.into(),
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
        fn check(&self, reader: &BundleReader<'_>) -> Verdict {
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
        let Parsed { bundle: raw, .. } = hardy_bpv7::parse::parse(data).unwrap();
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
}
