//! The chain runner: executes the frozen per-hook chains inline.
//!
//! Filter invocations are synchronous and the reader borrows the caller's
//! decoded bundle, buffer, BCB OperationSets, and key source — borrows that
//! cannot cross a spawn boundary — so every chain runs inline on the calling
//! task. An empty chain costs one branch: nothing is parsed and nothing is
//! allocated.
//!
//! The output hooks' Rewriters edit through handles on the attempt's one
//! editor, which the output door owns and rebuilds once, after its own
//! stages. A Rewriter reads its predecessors' edits from a snapshot of the
//! editor's staged edits, taken after each link that edits: a header-sized
//! copy, not a materialisation of the bundle.
//!
//! Every runner returns the bundle to the caller with its outcome, or edits
//! the caller's editor, so a claimed bundle's status is always resolved by
//! the site that claimed it: no re-fetch, no restore path. No chain has an
//! error path. The input runners decode nothing themselves, running on the
//! door's own header decode. The output chains decode only the BCB
//! operation sets the record's block index locates; those bytes were
//! validated at ingress, so a failure to decode them is a BPA bug or storage
//! corruption, processing beyond it is undefined, and the engine panics,
//! which aborts the process — the fail-fast rule, by analogy with a storage
//! fault.

use hardy_bpv7::{
    block::Type,
    bpsec::{
        DecryptingReader, bcb,
        key::{KeySet, KeySource},
    },
    editor::Editor,
    eid::Eid,
    extension_editor::ExtensionEditor,
    status_report::ReasonCode,
};
// Aliased: the wire bundle, beside the record `Bundle` from `crate::bundle`.
use hardy_bpv7::Bundle as Bpv7Bundle;
use hardy_cbor::decode::parse_exact;
use tracing::{debug, error};

use super::{
    Boundary, ClassifyContext, RewriteContext, Verdict, VerifyContext,
    pack::chains::{FilterChains, InputChain, RewriterEntry, VerifierEntry},
};
use crate::{
    Bytes, HashMap,
    bundle::{Bundle, BundleMetadata, parse::peekable_payload},
    keys::KeyProvider,
};

// One spelling per hook, shared by the metric labels and diagnostics.
const INGRESS: &str = "ingress";
const ORIGINATE: &str = "originate";
const EGRESS: &str = "egress";
const DELIVER: &str = "deliver";

/// An input chain's verdict over a bundle, which travels back to its
/// claimant with it.
pub enum ChainOutcome {
    /// The bundle passed the chain, with the deltas its Classifiers applied.
    Continue(Bundle),
    /// A filter dropped the bundle, optionally with a status-report reason.
    Drop(Bundle, Option<ReasonCode>),
}

// The key source for one pass. A bundle without a BCB never consults keys —
// decrypted reads exist only for blocks under a BCB, and no stage adds one —
// so it skips the provider round-trip. One helper for every pass, so the
// input and output paths cannot diverge.
fn derive_keys(
    has_bcb: bool,
    bundle: &Bpv7Bundle,
    buf: &[u8],
    key_provider: &dyn KeyProvider,
) -> Box<dyn KeySource> {
    if has_bcb {
        key_provider.key_source(bundle, buf)
    } else {
        Box::new(KeySet::EMPTY)
    }
}

/// The key source for one output attempt, derived once over the stored
/// bundle's block index and resident bytes. No output stage adds a BCB — a
/// Rewriter cannot, and the scheduled removals and the per-hop writes only
/// remove operations — so these keys serve every view the attempt edits on
/// to.
pub(crate) fn output_keys(
    bundle: &Bpv7Bundle,
    data: &[u8],
    key_provider: &dyn KeyProvider,
) -> Box<dyn KeySource> {
    let has_bcb = bundle
        .blocks
        .values()
        .any(|block| block.block_type == Type::BlockSecurity);
    derive_keys(has_bcb, bundle, data, key_provider)
}

// The payload's resident prefix, for the contexts' `payload_peek`: none for a
// payload with no peekable prefix (`peekable_payload`). Compared in u64 first:
// the payload's range is wire-derived.
fn resident_payload_prefix<'a>(bundle: &Bundle, buf: &'a [u8]) -> Option<&'a [u8]> {
    let block = peekable_payload(&bundle.bpv7)?;
    let body = block.payload_range();
    let resident = buf.len() as u64;
    if body.start > resident {
        return None;
    }
    let start = usize::try_from(body.start).ok()?;
    let end = usize::try_from(body.end.min(resident)).ok()?;
    buf.get(start..end)
}

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

// An output chain's BCB operation sets failing to decode: their bytes were
// validated at ingress and come back from storage, so the failure is a BPA
// bug or storage corruption. Logged, then the panic that aborts the process.
#[cold]
fn output_undecodable(hook: &str, e: impl core::fmt::Debug) -> ! {
    error!("The {hook} chain's bytes do not decode: {e:?}");
    panic!("The {hook} chain's bytes do not decode: {e:?}")
}

// The BCB operation sets of a materialised bundle, decoded from the BCB
// blocks its block index locates: the output chains parse nothing else.
fn decode_bcbs(hook: &str, bundle: &Bpv7Bundle, data: &[u8]) -> HashMap<u64, bcb::OperationSet> {
    bundle
        .blocks
        .iter()
        .filter(|(_, block)| block.block_type == Type::BlockSecurity)
        .map(|(&number, block)| {
            let body = block
                .payload(data)
                .unwrap_or_else(|| output_undecodable(hook, "a BCB body is not resident"));
            let opset = parse_exact::<bcb::OperationSet>(body)
                .unwrap_or_else(|e| output_undecodable(hook, e));
            (number, opset)
        })
        .collect()
}

impl FilterChains {
    /// Runs the Ingress chain (Verifiers, then Classifiers) on the resident
    /// buffer `data`, with the BCB OperationSets the gate's header pass
    /// decoded. `data` is the resident prefix: the headers and whatever of the
    /// payload has arrived, at least the peek the gate holds. A filter reading
    /// a payload not all resident gets the reader's `NotResident`.
    #[allow(clippy::result_large_err)]
    pub(crate) fn run_ingress(
        &self,
        bundle: Bundle,
        data: Bytes,
        bcbs: &HashMap<u64, bcb::OperationSet>,
        key_provider: &dyn KeyProvider,
    ) -> ChainOutcome {
        if self.ingress.verifiers.is_empty() && self.ingress.classifiers.is_empty() {
            return ChainOutcome::Continue(bundle);
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
    /// the resident prefix at the originate door's admission stage, with
    /// the declared peek held, exactly as [`run_ingress`](Self::run_ingress)
    /// at the CLA gate.
    pub(crate) fn run_originate(
        &self,
        bundle: Bundle,
        data: Bytes,
        bcbs: &HashMap<u64, bcb::OperationSet>,
        key_provider: &dyn KeyProvider,
    ) -> ChainOutcome {
        if self.originate.verifiers.is_empty() && self.originate.classifiers.is_empty() {
            return ChainOutcome::Continue(bundle);
        }
        self.run_input_decoded(&self.originate, ORIGINATE, bundle, data, bcbs, key_provider)
    }

    /// Whether the Originate chain has any registered links, for the
    /// originate door's short-circuit — the twin of
    /// [`has_ingress`](Self::has_ingress).
    pub(crate) fn has_originate(&self) -> bool {
        !self.originate.verifiers.is_empty() || !self.originate.classifiers.is_empty()
    }

    /// Runs the Egress Rewriters on the attempt's editor, after the
    /// scheduled removals and before the per-hop writes, which the output
    /// door applies to the same editor. Nothing at Egress drops a bundle.
    /// Returns whether a link edited.
    pub(crate) fn run_egress(
        &self,
        editor: &mut Editor<'_>,
        metadata: &BundleMetadata,
        next_hop: &Eid,
        keys: &dyn KeySource,
    ) -> bool {
        if self.egress.is_empty() {
            return false;
        }
        rewrite(
            &self.egress,
            EGRESS,
            Boundary::Egress { next_hop },
            editor,
            metadata,
            keys,
        )
    }

    /// Runs the Deliver Rewriters on the attempt's editor, after the
    /// scheduled removals. Returns whether a link edited.
    pub(crate) fn run_deliver(
        &self,
        editor: &mut Editor<'_>,
        metadata: &BundleMetadata,
        keys: &dyn KeySource,
    ) -> bool {
        if self.deliver.rewriters.is_empty() {
            return false;
        }
        rewrite(
            &self.deliver.rewriters,
            DELIVER,
            Boundary::Deliver,
            editor,
            metadata,
            keys,
        )
    }

    /// Whether the Deliver chain has any registered Rewriters, for the
    /// delivery door's short-circuit: with none and nothing scheduled for
    /// removal, it builds no editor.
    pub(crate) fn has_deliver_rewriters(&self) -> bool {
        !self.deliver.rewriters.is_empty()
    }

    /// Runs the Deliver Verifiers over the bundle the Deliver Rewriters
    /// left, materialised: `Some` carries the dropping Verifier's reason,
    /// `None` means every Verifier passed.
    pub(crate) fn verify_deliver(
        &self,
        bundle: &Bundle,
        data: &[u8],
        keys: &dyn KeySource,
    ) -> Option<Option<ReasonCode>> {
        if self.deliver.verifiers.is_empty() {
            return None;
        }
        let bcbs = decode_bcbs(DELIVER, &bundle.bpv7, data);
        let reader = DecryptingReader::new(&bundle.bpv7.blocks, data, &bcbs, keys);
        check_verifiers(
            &self.deliver.verifiers,
            DELIVER,
            &VerifyContext::new(
                &bundle.bpv7,
                &reader,
                &bundle.metadata,
                resident_payload_prefix(bundle, data),
            ),
        )
    }

    // The Verifier-then-Classifier pass over a resident buffer whose BCB
    // OperationSets are already decoded — both input doors thread in the set
    // from their one header decode (`buf` is the resident prefix, and a
    // payload not all resident reads as the reader's `NotResident`).
    #[allow(clippy::result_large_err)]
    fn run_input_decoded(
        &self,
        chain: &InputChain,
        hook: &'static str,
        mut bundle: Bundle,
        buf: Bytes,
        bcbs: &HashMap<u64, bcb::OperationSet>,
        key_provider: &dyn KeyProvider,
    ) -> ChainOutcome {
        let keys = derive_keys(!bcbs.is_empty(), &bundle.bpv7, &buf, key_provider);

        // The reader lends the *wire* view only, so one reader — and its
        // decrypt memo — serves the whole pass: the delta applications
        // below touch `bundle.metadata`, a disjoint borrow.
        let reader = DecryptingReader::new(&bundle.bpv7.blocks, &buf, bcbs, &*keys);
        let peek = resident_payload_prefix(&bundle, &buf);

        if let Some(reason) = check_verifiers(
            &chain.verifiers,
            hook,
            &VerifyContext::new(&bundle.bpv7, &reader, &bundle.metadata, peek),
        ) {
            return ChainOutcome::Drop(bundle, reason);
        }

        for entry in chain.classifiers.iter() {
            // Each delta is applied before the next link runs: a Classifier
            // sees the metadata its predecessors wrote.
            let ctx = ClassifyContext::new(&bundle.bpv7, &reader, &bundle.metadata, peek);
            match entry.classifier.classify(&ctx) {
                Verdict::Continue(delta) => bundle.metadata.apply(delta),
                Verdict::Drop(reason) => {
                    debug!("Classifier '{}' dropped bundle: {reason:?}", entry.label);
                    metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
                    return ChainOutcome::Drop(bundle, reason);
                }
            }
        }

        ChainOutcome::Continue(bundle)
    }
}

// The Rewriter stage both output hooks share, on the attempt's editor. Each
// link edits through a handle on it and reads a snapshot of the edits its
// predecessors staged; a link that leaves the editor as it was shares its
// snapshot, and the reader's decrypt memo, with the next. A snapshot whose
// BCB operation sets do not decode is a BPA bug or storage corruption.
// Returns whether any link edited.
fn rewrite(
    rewriters: &[RewriterEntry],
    hook: &'static str,
    boundary: Boundary<'_>,
    editor: &mut Editor<'_>,
    metadata: &BundleMetadata,
    keys: &dyn KeySource,
) -> bool {
    let mut edited = false;
    let mut links = rewriters.iter().peekable();
    while links.peek().is_some() {
        let view = editor
            .staged_view()
            .unwrap_or_else(|e| output_undecodable(hook, e));
        let reader = view.reader(keys);
        for entry in links.by_ref() {
            let mut ctx = RewriteContext::new(
                view.bundle(),
                &reader,
                metadata,
                boundary,
                ExtensionEditor::new(editor),
            );
            entry.rewriter.rewrite(&mut ctx);
            if ctx.is_modified() {
                debug!("Rewriter '{}' edited the bundle", entry.label);
                metrics::counter!("bpa.filter.modified", "hook" => hook).increment(1);
                edited = true;
                break;
            }
        }
    }
    edited
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use alloc::borrow::Cow;

    use core::sync::atomic::AtomicBool;

    use hardy_bpv7::{
        block,
        builder::Builder,
        crc::CrcType,
        creation_timestamp::CreationTimestamp,
        editor::Chunk,
        extension_editor::Error,
        parse::{Parsed, parse},
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
        let ChainOutcome::Continue(bundle) =
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
        let ChainOutcome::Continue(bundle) =
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider)
        else {
            panic!("the Verifier must run before the Classifier writes the slot");
        };
        assert_eq!(bundle.metadata.slot(&MARK), Some(7));
    }

    // Chain order is registration order across packs too: a Classifier in
    // the first pack writes the slot an expecter in the second requires, so
    // the pair passes frozen in that order and drops frozen the other way.
    #[test]
    fn chain_order_spans_packs_in_registration_order() {
        let packs = || {
            let mut writer = FilterPack::new("writer");
            writer.ingress_classifier("writer", SlotWriter(&MARK, 7));
            let mut expecter = FilterPack::new("expecter");
            expecter.ingress_classifier("expecter", SlotExpecter(&MARK, 7));
            (writer, expecter)
        };

        let (writer, expecter) = packs();
        let chains = FilterChains::freeze(vec![writer, expecter]);
        let (bundle, data, bcbs) = test_bundle();
        assert!(matches!(
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider),
            ChainOutcome::Continue(..)
        ));

        let (writer, expecter) = packs();
        let chains = FilterChains::freeze(vec![expecter, writer]);
        let (bundle, data, bcbs) = test_bundle();
        assert!(matches!(
            chains.run_ingress(bundle, data, &bcbs, &NullKeyProvider),
            ChainOutcome::Drop(_, Some(ReasonCode::NoAdditionalInformation))
        ));
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
        let ChainOutcome::Drop(_, reason) =
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
        let ChainOutcome::Drop(_, reason) =
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
        let ChainOutcome::Continue(bundle) =
            chains.run_ingress(bundle, data.clone(), &bcbs, &NullKeyProvider)
        else {
            panic!("the shared Verifier passes at Ingress");
        };
        assert_eq!(
            chains.verify_deliver(&bundle, &data, &KeySet::EMPTY),
            None,
            "the shared Verifier passes at Deliver"
        );
        assert_eq!(shared.0.load(Ordering::Relaxed), 2);
    }

    const CUSTOM_BLOCK: block::Type = block::Type::Unrecognised(192);

    struct BlockInserter;

    impl Rewriter for BlockInserter {
        fn rewrite(&self, ctx: &mut RewriteContext<'_, '_>) {
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

    // The Deliver Rewriters on an editor over the record's block index, the
    // one rebuild when a link edited, then the Deliver Verifiers: the
    // sequence the delivery door runs.
    fn deliver(
        chains: &FilterChains,
        mut bundle: Bundle,
        data: Bytes,
    ) -> (Bundle, Bytes, Option<Option<ReasonCode>>) {
        let mut editor = Editor::new(&bundle.bpv7, &data);
        let edited = chains.run_deliver(&mut editor, &bundle.metadata, &KeySet::EMPTY);
        let rebuilt = edited.then(|| editor.rebuild_bundle().unwrap());
        let data = match rebuilt {
            Some((rebuilt, chunks)) => {
                bundle.bpv7.blocks = rebuilt.blocks;
                Chunk::flatten_bytes(chunks, data)
            }
            None => data,
        };
        let verdict = chains.verify_deliver(&bundle, &data, &KeySet::EMPTY);
        (bundle, data, verdict)
    }

    #[test]
    fn rewriter_edit_reaches_the_deliver_verifier_consistently() {
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("inserter", BlockInserter);
        pack.deliver_verifier("expecter", BlockExpecter);
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
        let (bundle, data, verdict) = deliver(&chains, bundle, data);
        assert_eq!(
            verdict, None,
            "the verifier must have seen the inserted block"
        );

        // The rebuilt pair reparses: the rewrite really is on the wire.
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

    // The Egress Rewriters edit the attempt's editor; its rebuild must hand
    // back a block map that indexes the rebuilt bytes.
    #[test]
    fn egress_edits_rebuild_consistently() {
        let mut pack = FilterPack::new("test");
        pack.egress_rewriter("inserter", BlockInserter);
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        let mut editor = Editor::new(&bundle.bpv7, &data);
        assert!(chains.run_egress(&mut editor, &bundle.metadata, &next_hop, &KeySet::EMPTY));
        let (rebuilt, chunks) = editor.rebuild_bundle().unwrap();
        let data = Chunk::flatten_bytes(chunks, data.clone());

        let block = rebuilt
            .blocks
            .values()
            .find(|b| b.block_type == CUSTOM_BLOCK)
            .expect("the insert is in the rebuilt block map");
        let body = block
            .payload(&data)
            .expect("the inserted block is resident");
        assert_eq!(body, emit(&42u64).0.as_slice());
        let payload = rebuilt.blocks[&1].payload(&data).unwrap();
        assert_eq!(payload, b"engine-test");
    }

    // Inserts the custom block, then records whether its own context shows
    // the insert: it must not, since a link reads its predecessors' edits.
    struct SelfObserver(Arc<AtomicBool>);

    impl Rewriter for SelfObserver {
        fn rewrite(&self, ctx: &mut RewriteContext<'_, '_>) {
            let _ = ctx.editor().insert(
                CUSTOM_BLOCK,
                block::Flags::default(),
                CrcType::None,
                emit(&42u64).0.into(),
            );
            let seen = ctx
                .bundle()
                .blocks
                .values()
                .any(|b| b.block_type == CUSTOM_BLOCK);
            self.0.store(seen, Ordering::Relaxed);
        }
    }

    // Records whether it reads its predecessor's insert, through both the
    // block headers and the reader.
    struct PredecessorObserver(Arc<AtomicBool>);

    impl Rewriter for PredecessorObserver {
        fn rewrite(&self, ctx: &mut RewriteContext<'_, '_>) {
            let read = ctx
                .bundle()
                .blocks
                .iter()
                .find(|(_, b)| b.block_type == CUSTOM_BLOCK)
                .and_then(|(&n, _)| ctx.reader().extract::<u64>(n).ok().flatten());
            self.0.store(read == Some(42), Ordering::Relaxed);
        }
    }

    #[test]
    fn a_rewriter_reads_its_predecessors_edits_not_its_own() {
        let saw_own = Arc::new(AtomicBool::new(true));
        let saw_predecessor = Arc::new(AtomicBool::new(false));
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("self", SelfObserver(saw_own.clone()));
        pack.deliver_rewriter("next", PredecessorObserver(saw_predecessor.clone()));
        let chains = freeze(pack);

        let (bundle, data, _) = test_bundle();
        let (bundle, _, _) = deliver(&chains, bundle, data);
        assert!(
            !saw_own.load(Ordering::Relaxed),
            "a link's own edit is not in its snapshot"
        );
        assert!(
            saw_predecessor.load(Ordering::Relaxed),
            "the next link reads the edit through the snapshot"
        );
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
        fn rewrite(&self, ctx: &mut RewriteContext<'_, '_>) {
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
        let mut editor = Editor::new(&bundle.bpv7, &data);
        assert!(
            !chains.run_deliver(&mut editor, &bundle.metadata, &KeySet::EMPTY),
            "a refused edit is not an edit"
        );
        // Nothing was edited: the editor rebuilds the bytes unchanged.
        let (_, chunks) = editor.rebuild_bundle().unwrap();
        assert_eq!(Chunk::flatten_bytes(chunks, data.clone()), data);
    }

    // Counts key-source derivations.
    struct CountingProvider(AtomicUsize);

    impl KeyProvider for CountingProvider {
        fn key_source(&self, _bundle: &Bpv7Bundle, _data: &[u8]) -> Box<dyn KeySource> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::new(KeySet::EMPTY)
        }
    }

    struct NoopRewriter;

    impl Rewriter for NoopRewriter {
        fn rewrite(&self, _ctx: &mut RewriteContext<'_, '_>) {}
    }

    struct PassVerifier;

    impl Verifier for PassVerifier {
        fn verify(&self, _ctx: &VerifyContext<'_>) -> Verdict {
            Verdict::Continue(())
        }
    }

    #[test]
    fn output_keys_skip_the_provider_without_a_bcb() {
        let (bundle, data, bcbs) = test_bundle();
        assert!(bcbs.is_empty(), "the fixture bundle carries no BCB");
        let provider = CountingProvider(AtomicUsize::new(0));
        let _keys = output_keys(&bundle.bpv7, &data, &provider);
        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            0,
            "a BPSec-free bundle never consults the key provider"
        );
    }

    // A bundle carrying a BCB (over its Hop Count block, under a generated
    // key), so a pass over it derives keys.
    #[cfg(feature = "rfc9173")]
    fn bcb_test_bundle() -> (Bundle, Bytes) {
        use core::num::NonZeroU8;

        use hardy_bpv7::{
            bpsec::{
                encryptor::{Context, Encryptor},
                key::{EncAlgorithm, Key, Operation, Type},
            },
            hop_info::HopInfo,
        };
        use rand::{TryRng, rngs::SysRng};

        // Immaterial key value: nothing decrypts, the bundle only has to
        // carry a BCB; generated per the no-literal-keys rule.
        let mut k = vec![0u8; 32];
        SysRng.try_fill_bytes(&mut k).unwrap();
        let key = Key {
            key_type: Type::octet_sequence(k),
            key_algorithm: None,
            enc_algorithm: Some(EncAlgorithm::A256GCM),
            operations: Some([Operation::Encrypt].into_iter().collect()),
            id: Some("ipn:1.1".into()),
            key_use: None,
        };
        let (built, data) = Builder::new("ipn:1.1".parse().unwrap(), "ipn:99.1".parse().unwrap())
            .with_hop_count(&HopInfo {
                limit: NonZeroU8::new(64).unwrap(),
                count: 1,
            })
            .with_payload(Cow::Borrowed(b"engine-test"))
            .build(CreationTimestamp::now())
            .unwrap();
        let hop_count = *built
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block::Type::HopCount)
            .unwrap()
            .0;
        let encrypted = Encryptor::new(&built, &data)
            .encrypt_block(
                hop_count,
                Context::AES_GCM(Default::default()),
                "ipn:1.1".parse().unwrap(),
                &key,
            )
            .map_err(|(_, e)| e)
            .unwrap()
            .rebuild()
            .unwrap();
        let Parsed {
            bundle, data, bcbs, ..
        } = parse(Bytes::from(encrypted)).unwrap();
        assert!(!bcbs.is_empty(), "the bundle carries a BCB");
        (
            Bundle {
                bpv7: bundle,
                metadata: BundleMetadata::originated(),
                status: BundleStatus::New,
            },
            data,
        )
    }

    #[cfg(feature = "rfc9173")]
    #[test]
    fn output_keys_consult_the_provider_with_a_bcb() {
        let (bundle, data) = bcb_test_bundle();
        let provider = CountingProvider(AtomicUsize::new(0));
        let _keys = output_keys(&bundle.bpv7, &data, &provider);
        assert_eq!(provider.0.load(Ordering::Relaxed), 1);
    }

    // The BCB fixture with its BCB's body overwritten by a CBOR integer, so
    // the operation set no longer decodes; the block index is unchanged.
    #[cfg(feature = "rfc9173")]
    fn corrupt_bcb_bundle() -> (Bundle, Bytes) {
        let (bundle, data) = bcb_test_bundle();
        let range = bundle
            .bpv7
            .blocks
            .values()
            .find(|b| b.block_type == block::Type::BlockSecurity)
            .expect("the fixture carries a BCB")
            .payload_range();
        let mut corrupt = data.to_vec();
        corrupt[usize::try_from(range.start).unwrap()] = 0x00;
        (bundle, Bytes::from(corrupt))
    }

    // An output chain decodes only the BCB operation sets its index locates;
    // those bytes were validated at ingress, so failing to decode them is a
    // BPA bug or storage corruption: fatal, never a park. The panic is caught
    // here because the chain runs outside a pipeline task.
    #[cfg(feature = "rfc9173")]
    #[test]
    #[should_panic(expected = "The egress chain's bytes do not decode")]
    fn an_undecodable_bcb_is_fatal_at_egress() {
        let mut pack = FilterPack::new("test");
        pack.egress_rewriter("noop", NoopRewriter);
        let chains = freeze(pack);

        let (bundle, data) = corrupt_bcb_bundle();
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        let mut editor = Editor::new(&bundle.bpv7, &data);
        chains.run_egress(&mut editor, &bundle.metadata, &next_hop, &KeySet::EMPTY);
    }

    #[cfg(feature = "rfc9173")]
    #[test]
    #[should_panic(expected = "The deliver chain's bytes do not decode")]
    fn an_undecodable_bcb_is_fatal_at_deliver_verification() {
        let mut pack = FilterPack::new("test");
        pack.deliver_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data) = corrupt_bcb_bundle();
        chains.verify_deliver(&bundle, &data, &KeySet::EMPTY);
    }

    #[test]
    fn bpsec_free_input_skips_key_derivation() {
        let mut pack = FilterPack::new("test");
        pack.ingress_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data, bcbs) = test_bundle();
        assert!(bcbs.is_empty(), "the fixture bundle carries no BCB");
        let provider = CountingProvider(AtomicUsize::new(0));
        let ChainOutcome::Continue(..) = chains.run_ingress(bundle, data, &bcbs, &provider) else {
            panic!("a passing Verifier must continue");
        };
        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            0,
            "a BPSec-free bundle never consults the key provider"
        );
    }
}
