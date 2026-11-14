//! The chain runner: executes the frozen per-hook chains inline.
//!
//! Filter invocations are synchronous and the reader borrows the caller's
//! decoded bundle, buffer, BCB OperationSets, and key source — borrows that
//! cannot cross a spawn boundary — so every chain runs inline on the calling
//! task. An empty chain costs one branch: nothing is parsed and nothing is
//! allocated.
//!
//! The output hooks' Rewriters edit through handles on the attempt's one
//! editor; see the [filter module docs](super#reading-the-bundle).
//!
//! Every input runner hands its verdict back to the door that holds the
//! arrival, and every output runner edits the door's editor, so a claimed
//! bundle's status is always resolved by the site that claimed it: no
//! re-fetch, no restore path. No chain has an error path. The input runners
//! decode nothing themselves, running on the door's own header decode. The
//! output chains decode only the BCB operation sets the record's block
//! index locates; those bytes were validated at ingress, so a failure to
//! decode them is a BPA bug or storage corruption, processing beyond it is
//! undefined, and the engine panics, which aborts the process — the
//! fail-fast rule, by analogy with a storage fault.

use core::{fmt::Debug, mem::take, ops::ControlFlow};

// Aliased: the wire bundle, beside the record `Bundle`.
use hardy_bpv7::{
    Bundle as Bpv7Bundle, Error as Bpv7Error,
    block::{Block, Type},
    bpsec::{Decrypted, DecryptingReader, Roles, bcb, key::KeySource},
    editor::Editor,
    eid::Eid,
    parse::Parsed,
    reader::Reader,
    status_report::ReasonCode,
};
use hardy_cbor::decode::parse_exact;
use tracing::{debug, error};

use super::{
    Boundary, ClassifyContext, RewriteContext, Verdict, VerifyContext,
    pack::chains::{FilterChains, InputChain, RewriterEntry, VerifierEntry},
};
use crate::{
    HashMap,
    bundle::{BundleMetadata, arrival::peekable_payload},
    keys::{KeyProvider, derive_keys},
};

// One spelling per hook, shared by the metric labels and diagnostics.
const INGRESS: &str = "ingress";
const ORIGINATE: &str = "originate";
const EGRESS: &str = "egress";
const DELIVER: &str = "deliver";

// The payload's resident prefix, for the contexts' `payload_peek`: none for a
// payload with no peekable prefix (`peekable_payload`). Compared in u64 first:
// the payload's range is wire-derived.
fn resident_payload_prefix<'a>(bundle: &Bpv7Bundle, buf: &'a [u8]) -> Option<&'a [u8]> {
    let block = peekable_payload(bundle)?;
    let body = block.payload_range();
    let resident = buf.len() as u64;
    if body.start > resident {
        return None;
    }
    let start = usize::try_from(body.start).ok()?;
    let end = usize::try_from(body.end.min(resident)).ok()?;
    buf.get(start..end)
}

/// The engine's one spelling of the Verifier pass: the first Drop verdict,
/// or `Continue` when every Verifier passed.
fn check_verifiers(
    verifiers: &[VerifierEntry],
    hook: &'static str,
    ctx: &VerifyContext<'_>,
) -> Verdict {
    for entry in verifiers {
        if let Verdict::Drop(reason) = entry.verifier.verify(ctx) {
            debug!("Verifier '{}' dropped bundle: {reason:?}", entry.label);
            metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
            return Verdict::Drop(reason);
        }
    }
    Verdict::Continue(())
}

// An output chain's BCB operation sets failing to decode: their bytes were
// validated at ingress and come back from storage, so the failure is a BPA
// bug or storage corruption. Logged, then the panic that aborts the process.
// Cold, and the only place the message is formatted, so the success path
// pays for no diagnostics.
#[cold]
fn output_undecodable(hook: &str, e: impl Debug) -> ! {
    error!("The {hook} chain's bytes do not decode: {e:?}");
    panic!("The {hook} chain's bytes do not decode: {e:?}")
}

// The BCB operation sets of the BCB blocks `blocks` locates, read through
// `bodies`: the output chains parse nothing but these.
fn decode_bcbs<'r>(
    hook: &str,
    blocks: &HashMap<u64, Block>,
    bodies: &'r dyn Reader<'r>,
) -> HashMap<u64, bcb::OperationSet> {
    blocks
        .iter()
        .filter(|(_, block)| block.block_type == Type::BlockSecurity)
        .map(|(&number, _)| {
            let body = bodies
                .block(number)
                .and_then(|(_, availability)| availability.available())
                .unwrap_or_else(|| output_undecodable(hook, Bpv7Error::Altered));
            let opset = parse_exact::<bcb::OperationSet>(body.as_ref())
                .unwrap_or_else(|e| output_undecodable(hook, e));
            (number, opset)
        })
        .collect()
}

impl FilterChains {
    /// Runs the Ingress chain (Verifiers, then Classifiers) on the gate's
    /// header pass: its parse, whose `data` is the resident prefix (the
    /// headers and whatever of the payload has arrived, at least the peek
    /// the gate holds) and whose plaintext cache the pass filled, the key
    /// source it derived, and the arrival's `metadata`, which the
    /// Classifiers write. A filter reading a payload not all resident gets
    /// the reader's `NotResident`. The chain's reader is seeded from the
    /// parse's cache, so a block the header pass decrypted is not decrypted
    /// again, and a passing chain hands back to it what its filters
    /// decrypted. The security roles its
    /// Classifiers announce this node takes on the blocks
    /// ([`MetadataDelta::roles`](super::slots::MetadataDelta::roles)) collect
    /// in `roles`, a union across the chain, for the gate. Breaks with a
    /// filter's drop, carrying the reason a report would, if any.
    pub(crate) fn run_ingress(
        &self,
        parsed: &mut Parsed,
        metadata: &mut BundleMetadata,
        keys: &dyn KeySource,
        roles: &mut HashMap<u64, Roles>,
    ) -> ControlFlow<Option<ReasonCode>> {
        if self.ingress.verifiers.is_empty() && self.ingress.classifiers.is_empty() {
            return ControlFlow::Continue(());
        }
        self.run_input_decoded(
            &self.ingress,
            INGRESS,
            &parsed.bundle,
            metadata,
            &parsed.data,
            &parsed.bcbs,
            keys,
            &mut parsed.decrypted,
            roles,
        )
    }

    /// Whether the Ingress chain has any registered links. The streaming gate
    /// checks this to skip the pre-drain header re-decode and clone entirely
    /// when nothing would run.
    pub(crate) fn has_ingress(&self) -> bool {
        !self.ingress.verifiers.is_empty() || !self.ingress.classifiers.is_empty()
    }

    /// Runs the Originate chain (Verifiers, then Classifiers) on the
    /// resident buffer `head` and its already-decoded BCB OperationSets —
    /// the resident prefix at the originate door's admission stage, with
    /// the declared peek held, exactly as [`run_ingress`](Self::run_ingress)
    /// at the CLA gate — with the record's `metadata`, which the
    /// Classifiers write. Breaks with a filter's drop, carrying the reason a
    /// report would, if any.
    pub(crate) fn run_originate(
        &self,
        bundle: &Bpv7Bundle,
        metadata: &mut BundleMetadata,
        head: &[u8],
        bcbs: &HashMap<u64, bcb::OperationSet>,
        key_provider: &dyn KeyProvider,
    ) -> ControlFlow<Option<ReasonCode>> {
        if self.originate.verifiers.is_empty() && self.originate.classifiers.is_empty() {
            return ControlFlow::Continue(());
        }
        let keys = derive_keys(bundle, head, key_provider);
        // A source accepts nothing, so the blocks its Classifiers announce
        // are passed over.
        self.run_input_decoded(
            &self.originate,
            ORIGINATE,
            bundle,
            metadata,
            head,
            bcbs,
            &*keys,
            &mut Decrypted::default(),
            &mut HashMap::new(),
        )
    }

    /// Whether the Originate chain has any registered links, for the
    /// originate door's short-circuit — the twin of
    /// [`has_ingress`](Self::has_ingress).
    pub(crate) fn has_originate(&self) -> bool {
        !self.originate.verifiers.is_empty() || !self.originate.classifiers.is_empty()
    }

    /// Runs the Egress Rewriters on the attempt's editor, before the
    /// per-hop writes, which the output door applies to the same editor.
    /// Nothing at Egress drops a bundle.
    pub fn rewrite_egress(
        &self,
        editor: &mut Editor<'_>,
        metadata: &BundleMetadata,
        next_hop: &Eid,
        keys: &dyn KeySource,
    ) {
        if self.egress.is_empty() {
            return;
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

    /// Whether any Rewriter is registered at Egress.
    pub fn has_egress_rewriters(&self) -> bool {
        !self.egress.is_empty()
    }

    /// Runs the Deliver Rewriters on the attempt's editor.
    pub fn rewrite_deliver(
        &self,
        editor: &mut Editor<'_>,
        metadata: &BundleMetadata,
        keys: &dyn KeySource,
    ) {
        if self.deliver.rewriters.is_empty() {
            return;
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

    /// Whether any Rewriter is registered at Deliver.
    pub fn has_deliver_rewriters(&self) -> bool {
        !self.deliver.rewriters.is_empty()
    }

    /// Whether any Verifier is registered at Deliver.
    pub fn has_deliver_verifiers(&self) -> bool {
        !self.deliver.verifiers.is_empty()
    }

    /// Runs the Deliver Verifiers over the bundle as the attempt's `editor`
    /// leaves it, after the Deliver Rewriters: its current bundle and a
    /// decrypting reader over it, with the payload's resident prefix in
    /// `data`, the bytes the editor works over, as their peek. Nothing is
    /// materialised.
    pub fn verify_deliver(
        &self,
        editor: &Editor<'_>,
        data: &[u8],
        metadata: &BundleMetadata,
        keys: &dyn KeySource,
    ) -> Verdict {
        if self.deliver.verifiers.is_empty() {
            return Verdict::Continue(());
        }
        let current = editor.current_bundle();
        let bcbs = decode_bcbs(DELIVER, &current.blocks, editor);
        let reader = DecryptingReader::over_editor(&current.blocks, editor, &bcbs, keys);
        check_verifiers(
            &self.deliver.verifiers,
            DELIVER,
            &VerifyContext::new(
                &current,
                &reader,
                metadata,
                resident_payload_prefix(&current, data),
            ),
        )
    }

    // The Verifier-then-Classifier pass over a resident buffer whose BCB
    // OperationSets are already decoded — both input doors thread in the set
    // from their one header decode (`buf` is the resident prefix, and a
    // payload not all resident reads as the reader's `NotResident`). The
    // reader's memo starts from `decrypted` and goes back into it when the
    // chain passes. The security roles the Classifiers announce this node
    // takes collect in `roles`, a union, never into the persisted record.
    #[allow(clippy::too_many_arguments)]
    fn run_input_decoded(
        &self,
        chain: &InputChain,
        hook: &'static str,
        bundle: &Bpv7Bundle,
        metadata: &mut BundleMetadata,
        buf: &[u8],
        bcbs: &HashMap<u64, bcb::OperationSet>,
        keys: &dyn KeySource,
        decrypted: &mut Decrypted,
        roles: &mut HashMap<u64, Roles>,
    ) -> ControlFlow<Option<ReasonCode>> {
        // The reader lends the *wire* view only, so one reader — and its
        // decrypt memo — serves the whole pass: the delta applications
        // below write `metadata`, which the contexts only borrow.
        let reader =
            DecryptingReader::new(&bundle.blocks, buf, bcbs, keys).with_decrypted(take(decrypted));
        let peek = resident_payload_prefix(bundle, buf);

        if let Verdict::Drop(reason) = check_verifiers(
            &chain.verifiers,
            hook,
            &VerifyContext::new(bundle, &reader, metadata, peek),
        ) {
            return ControlFlow::Break(reason);
        }

        for entry in chain.classifiers.iter() {
            // Each delta is applied before the next link runs: a Classifier
            // sees the metadata its predecessors wrote.
            let ctx = ClassifyContext::new(bundle, &reader, metadata, peek);
            match entry.classifier.classify(&ctx) {
                Verdict::Continue(mut delta) => {
                    for (n, announced) in take(&mut delta.roles) {
                        *roles.entry(n).or_default() |= announced;
                    }
                    metadata.apply(delta);
                }
                Verdict::Drop(reason) => {
                    debug!("Classifier '{}' dropped bundle: {reason:?}", entry.label);
                    metrics::counter!("bpa.filter.filtered", "hook" => hook).increment(1);
                    return ControlFlow::Break(reason);
                }
            }
        }

        *decrypted = reader.into_decrypted();
        ControlFlow::Continue(())
    }
}

// The Rewriter stage both output hooks share, on the attempt's editor. Each
// link edits through a handle on it and reads the bundle as the links
// before it left it: a header-sized copy of the editor's current bundle,
// re-taken after a link that edits, and a decrypting reader over the
// editor, taken on demand. The BCB operation sets are decoded once: no
// Rewriter can add, remove or edit a BCB.
fn rewrite(
    rewriters: &[RewriterEntry],
    hook: &'static str,
    boundary: Boundary<'_>,
    editor: &mut Editor<'_>,
    metadata: &BundleMetadata,
    keys: &dyn KeySource,
) {
    let mut current = editor.current_bundle();
    let bcbs = decode_bcbs(hook, &current.blocks, &*editor);
    for entry in rewriters {
        let modified = {
            let mut ctx =
                RewriteContext::new(&current, metadata, boundary, &mut *editor, &bcbs, keys);
            entry.rewriter.rewrite(&mut ctx);
            ctx.is_modified()
        };
        if modified {
            debug!("Rewriter '{}' edited the bundle", entry.label);
            metrics::counter!("bpa.filter.modified", "hook" => hook).increment(1);
            current = editor.current_bundle();
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "rfc9173")]
    use core::num::NonZeroU8;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use alloc::borrow::Cow;

    use hardy_bpv7::{
        block,
        bpsec::key::KeySet,
        builder::Builder,
        crc::CrcType,
        creation_timestamp::CreationTimestamp,
        editor::Chunk,
        extension_editor::Error,
        parse::parse,
        reader::{Availability, ReaderExt},
        status_report::ReasonCode,
    };
    #[cfg(feature = "rfc9173")]
    use hardy_bpv7::{
        bpsec::{
            encryptor::{Context, Encryptor},
            key::{EncAlgorithm, Key, Operation, Type},
        },
        hop_info::HopInfo,
    };
    use hardy_cbor::encode::emit;
    #[cfg(feature = "rfc9173")]
    use rand::{TryRng, rngs::SysRng};

    use super::*;
    use crate::{
        Arc, Bytes,
        bundle::{Bundle, BundleMetadata, BundleStatus},
        filter::{
            Classifier, Rewriter, Verifier,
            pack::{FilterPack, chains::FilterChains},
            slots::{MetadataDelta, Slot},
        },
    };

    // An arrival as the ingress gate holds one: the parse, and the
    // metadata the chain writes.
    fn test_arrival() -> (Parsed, BundleMetadata) {
        let (_, data) = Builder::new("ipn:1.1".parse().unwrap(), "ipn:99.1".parse().unwrap())
            .with_payload(Cow::Borrowed(b"engine-test"))
            .build(CreationTimestamp::now())
            .unwrap();
        (
            parse(Bytes::from(data)).unwrap(),
            BundleMetadata::originated(),
        )
    }

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

        let (mut parsed, mut metadata) = test_arrival();
        let ControlFlow::Continue(()) = chains.run_ingress(
            &mut parsed,
            &mut metadata,
            &KeySet::EMPTY,
            &mut HashMap::new(),
        ) else {
            panic!("expecter must have seen the writer's delta");
        };
        assert_eq!(metadata.slot(&MARK), Some(7));
    }

    // Announces security roles on blocks.
    struct Announces(Vec<(u64, Roles)>);

    impl Classifier for Announces {
        fn classify(&self, _ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
            let mut delta = MetadataDelta::default();
            delta.roles.extend(self.0.iter().copied());
            Verdict::Continue(delta)
        }
    }

    // The roles the Ingress chain's Classifiers announce accumulate as a
    // union for the gate: a later link adds to an earlier one's, and on one
    // operation the acceptor's role outranks the verifier's.
    #[test]
    fn announced_roles_accumulate_for_the_gate() {
        use hardy_bpv7::bpsec::Role::{Acceptor, Verifier};
        let mut pack = FilterPack::new("test");
        pack.ingress_classifier(
            "two",
            Announces(vec![
                (
                    2,
                    Roles {
                        confidentiality: Some(Verifier),
                        integrity: Some(Acceptor),
                    },
                ),
                (
                    4,
                    Roles {
                        confidentiality: Some(Verifier),
                        integrity: None,
                    },
                ),
            ]),
        );
        pack.ingress_classifier(
            "three",
            Announces(vec![
                (
                    2,
                    Roles {
                        confidentiality: Some(Acceptor),
                        integrity: Some(Verifier),
                    },
                ),
                (
                    3,
                    Roles {
                        confidentiality: None,
                        integrity: Some(Acceptor),
                    },
                ),
            ]),
        );
        let chains = freeze(pack);

        let (mut parsed, mut metadata) = test_arrival();
        let mut roles = HashMap::new();
        let ControlFlow::Continue(()) =
            chains.run_ingress(&mut parsed, &mut metadata, &KeySet::EMPTY, &mut roles)
        else {
            panic!("announcing Classifiers continue");
        };
        assert_eq!(
            roles,
            [
                (
                    2,
                    Roles {
                        confidentiality: Some(Acceptor),
                        integrity: Some(Acceptor),
                    },
                ),
                (
                    3,
                    Roles {
                        confidentiality: None,
                        integrity: Some(Acceptor),
                    },
                ),
                (
                    4,
                    Roles {
                        confidentiality: Some(Verifier),
                        integrity: None,
                    },
                ),
            ]
            .into_iter()
            .collect::<HashMap<u64, Roles>>()
        );
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

        let (mut parsed, mut metadata) = test_arrival();
        let ControlFlow::Continue(()) = chains.run_ingress(
            &mut parsed,
            &mut metadata,
            &KeySet::EMPTY,
            &mut HashMap::new(),
        ) else {
            panic!("the Verifier must run before the Classifier writes the slot");
        };
        assert_eq!(metadata.slot(&MARK), Some(7));
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
        let (mut parsed, mut metadata) = test_arrival();
        assert!(matches!(
            chains.run_ingress(
                &mut parsed,
                &mut metadata,
                &KeySet::EMPTY,
                &mut HashMap::new()
            ),
            ControlFlow::Continue(())
        ));

        let (writer, expecter) = packs();
        let chains = FilterChains::freeze(vec![expecter, writer]);
        let (mut parsed, mut metadata) = test_arrival();
        assert!(matches!(
            chains.run_ingress(
                &mut parsed,
                &mut metadata,
                &KeySet::EMPTY,
                &mut HashMap::new()
            ),
            ControlFlow::Break(Some(ReasonCode::NoAdditionalInformation))
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

        let (mut parsed, mut metadata) = test_arrival();
        let ControlFlow::Break(reason) = chains.run_ingress(
            &mut parsed,
            &mut metadata,
            &KeySet::EMPTY,
            &mut HashMap::new(),
        ) else {
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

        let (mut parsed, mut metadata) = test_arrival();
        let ControlFlow::Break(reason) = chains.run_ingress(
            &mut parsed,
            &mut metadata,
            &KeySet::EMPTY,
            &mut HashMap::new(),
        ) else {
            panic!("the boxed Verifier must run");
        };
        assert_eq!(reason, Some(ReasonCode::BlockUnintelligible));

        // One instance behind an `Arc` serves two hooks.
        let shared = Arc::new(CountingVerifier(AtomicUsize::new(0)));
        let mut pack = FilterPack::new("test");
        pack.ingress_verifier("shared", shared.clone());
        pack.deliver_verifier("shared", shared.clone());
        let chains = freeze(pack);

        let (mut parsed, mut metadata) = test_arrival();
        let ControlFlow::Continue(()) = chains.run_ingress(
            &mut parsed,
            &mut metadata,
            &KeySet::EMPTY,
            &mut HashMap::new(),
        ) else {
            panic!("the shared Verifier passes at Ingress");
        };
        let (bundle, data, _) = test_bundle();
        assert_eq!(
            chains.verify_deliver(
                &Editor::new(&bundle.bpv7, &data),
                &data,
                &bundle.metadata,
                &KeySet::EMPTY
            ),
            Verdict::Continue(()),
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
    // Deliver Verifiers over its edits, then a rebuild when a link edited,
    // so the test can read what reached the wire.
    fn deliver(chains: &FilterChains, mut bundle: Bundle, data: Bytes) -> (Bundle, Bytes, Verdict) {
        let mut editor = Editor::new(&bundle.bpv7, &data);
        chains.rewrite_deliver(&mut editor, &bundle.metadata, &KeySet::EMPTY);
        let verdict = chains.verify_deliver(&editor, &data, &bundle.metadata, &KeySet::EMPTY);
        let rebuilt = editor
            .is_modified()
            .then(|| editor.rebuild_bundle().unwrap());
        let data = match rebuilt {
            Some((rebuilt, chunks)) => {
                bundle.bpv7.blocks = rebuilt.blocks;
                Chunk::flatten_bytes(chunks, data)
            }
            None => data,
        };
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
            verdict,
            Verdict::Continue(()),
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
        chains.rewrite_egress(&mut editor, &bundle.metadata, &next_hop, &KeySet::EMPTY);
        assert!(editor.is_modified(), "the Rewriter edited");
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
            "a link's own edit is not in its copy of the bundle"
        );
        assert!(
            saw_predecessor.load(Ordering::Relaxed),
            "the next link reads the edit through its copy and its reader"
        );
        assert!(
            bundle
                .bpv7
                .blocks
                .values()
                .any(|b| b.block_type == CUSTOM_BLOCK)
        );
    }

    // The Deliver peek P is the largest declared across packs, and a
    // Verifier registered without one declares 0.
    #[test]
    fn the_deliver_peek_is_the_largest_declared() {
        let mut first = FilterPack::new("first");
        first.deliver_verifier_with_peek("small", PassVerifier, 16);
        first.deliver_verifier("none", PassVerifier);
        let mut second = FilterPack::new("second");
        second.deliver_verifier_with_peek("large", PassVerifier, 64);
        let chains = FilterChains::freeze(vec![first, second]);
        assert_eq!(chains.deliver.peek, 64);
        assert_eq!(chains.deliver.verifiers.len(), 3);
        assert_eq!(chains.ingress.peek, 0);
    }

    struct PayloadAttacker;

    impl Rewriter for PayloadAttacker {
        fn rewrite(&self, ctx: &mut RewriteContext<'_, '_>) {
            let mut editor = ctx.editor();
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
        let (_, out, verdict) = deliver(&chains, bundle, data.clone());
        assert_eq!(verdict, Verdict::Continue(()), "a Rewriter has no verdict");
        // Nothing was edited: the bytes pass through unchanged.
        assert_eq!(out, data);
    }

    // Counts key-source derivations: an output attempt derives its keys
    // once, over the stored bundle, however many links run.
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

    // Two non-editing Rewriters and a Verifier at Deliver.
    fn passing_deliver_chain() -> FilterChains {
        let mut pack = FilterPack::new("test");
        pack.deliver_rewriter("noop-a", NoopRewriter);
        pack.deliver_rewriter("noop-b", NoopRewriter);
        pack.deliver_verifier("pass", PassVerifier);
        freeze(pack)
    }

    // The Deliver Rewriters on an editor with `provider`'s keys, then the
    // Deliver Verifiers over the editor, which the non-editing links leave
    // as it is.
    fn deliver_with(
        chains: &FilterChains,
        bundle: &Bundle,
        data: &Bytes,
        provider: &dyn KeyProvider,
    ) {
        let keys = derive_keys(&bundle.bpv7, data, provider);
        let mut editor = Editor::new(&bundle.bpv7, data);
        chains.rewrite_deliver(&mut editor, &bundle.metadata, &*keys);
        assert!(
            !editor.is_modified(),
            "the non-editing links leave the editor as it was"
        );
        assert_eq!(
            chains.verify_deliver(&editor, data, &bundle.metadata, &*keys),
            Verdict::Continue(())
        );
    }

    #[test]
    fn bpsec_free_output_skips_key_derivation() {
        let chains = passing_deliver_chain();
        let (bundle, data, bcbs) = test_bundle();
        assert!(bcbs.is_empty(), "the fixture bundle carries no BCB");
        let provider = CountingProvider(AtomicUsize::new(0));
        deliver_with(&chains, &bundle, &data, &provider);
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
        let mut encryptor = Encryptor::new(&built, &data);
        encryptor
            .encrypt_block(
                hop_count,
                Context::AES_GCM(Default::default()),
                "ipn:1.1".parse().unwrap(),
                &key,
            )
            .unwrap();
        let encrypted = encryptor.rebuild().unwrap();
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
    fn output_attempt_derives_keys_once() {
        let chains = passing_deliver_chain();
        let (bundle, data) = bcb_test_bundle();
        let provider = CountingProvider(AtomicUsize::new(0));
        deliver_with(&chains, &bundle, &data, &provider);
        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            1,
            "two non-editing Rewriters and a Verifier share one derivation"
        );
    }

    // The BCB of `bcb_test_bundle`, its body overwritten with bytes that do
    // not decode as an operation set, under the block index that still
    // locates it.
    #[cfg(feature = "rfc9173")]
    fn undecodable_bcb_bundle() -> (Bundle, Bytes) {
        let (bundle, data) = bcb_test_bundle();
        let body = bundle
            .bpv7
            .blocks
            .values()
            .find(|b| b.block_type == block::Type::BlockSecurity)
            .expect("the fixture carries a BCB")
            .payload_range();
        let body = usize::try_from(body.start).unwrap()..usize::try_from(body.end).unwrap();
        let mut bytes = data.to_vec();
        bytes[body].fill(0xff);
        (bundle, Bytes::from(bytes))
    }

    // An output chain's BCB operation sets were validated at ingress, so
    // failing to decode one is a BPA bug or storage corruption: fatal, never
    // a park. The panic is caught here because the chain runs outside a
    // pipeline task.
    #[cfg(feature = "rfc9173")]
    #[test]
    #[should_panic(expected = "The egress chain's bytes do not decode")]
    fn an_undecodable_bcb_is_fatal_at_egress() {
        let mut pack = FilterPack::new("test");
        pack.egress_rewriter("noop", NoopRewriter);
        let chains = freeze(pack);

        let (bundle, data) = undecodable_bcb_bundle();
        let next_hop: Eid = "ipn:2.0".parse().unwrap();
        let mut editor = Editor::new(&bundle.bpv7, &data);
        chains.rewrite_egress(&mut editor, &bundle.metadata, &next_hop, &KeySet::EMPTY);
    }

    #[cfg(feature = "rfc9173")]
    #[test]
    #[should_panic(expected = "The deliver chain's bytes do not decode")]
    fn an_undecodable_bcb_is_fatal_at_the_deliver_verifiers() {
        let mut pack = FilterPack::new("test");
        pack.deliver_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (bundle, data) = undecodable_bcb_bundle();
        chains.verify_deliver(
            &Editor::new(&bundle.bpv7, &data),
            &data,
            &bundle.metadata,
            &KeySet::EMPTY,
        );
    }

    #[test]
    fn bpsec_free_input_skips_key_derivation() {
        let mut pack = FilterPack::new("test");
        pack.originate_verifier("pass", PassVerifier);
        let chains = freeze(pack);

        let (mut bundle, data, bcbs) = test_bundle();
        assert!(bcbs.is_empty(), "the fixture bundle carries no BCB");
        let provider = CountingProvider(AtomicUsize::new(0));
        let ControlFlow::Continue(()) =
            chains.run_originate(&bundle.bpv7, &mut bundle.metadata, &data, &bcbs, &provider)
        else {
            panic!("a passing Verifier must continue");
        };
        assert_eq!(
            provider.0.load(Ordering::Relaxed),
            0,
            "a BPSec-free bundle never consults the key provider"
        );
    }

    // A key source that holds no keys and counts the lookups made of it.
    #[cfg(feature = "rfc9173")]
    struct CountingKeys(AtomicUsize);

    #[cfg(feature = "rfc9173")]
    impl KeySource for CountingKeys {
        fn key<'a>(&'a self, _source: &Eid, _operations: &[Operation]) -> Option<&'a Key> {
            self.0.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    // Reads one block and requires its plaintext.
    #[cfg(feature = "rfc9173")]
    struct ReadsBlock {
        block: u64,
        expected: Vec<u8>,
    }

    #[cfg(feature = "rfc9173")]
    impl Classifier for ReadsBlock {
        fn classify(&self, ctx: &ClassifyContext<'_>) -> Verdict<MetadataDelta> {
            match ctx.reader().block(self.block) {
                Some((_, Availability::Available(plaintext))) => {
                    assert_eq!(plaintext.as_ref(), self.expected.as_slice());
                }
                other => panic!("block {} must read as plaintext, not {other:?}", self.block),
            }
            Verdict::Continue(MetadataDelta::default())
        }
    }

    // The Ingress chain reads a block the header pass decrypted from the
    // arrival's cache, without a key lookup, and hands the cache back.
    #[cfg(feature = "rfc9173")]
    #[test]
    fn ingress_chain_reads_the_header_pass_plaintext() {
        let (bundle, data) = bcb_test_bundle();
        let mut parsed = parse(data).unwrap();
        let mut metadata = bundle.metadata;
        let hop_count = *parsed
            .bundle
            .blocks
            .iter()
            .find(|(_, b)| b.block_type == block::Type::HopCount)
            .expect("the fixture carries a Hop Count")
            .0;
        let plaintext = emit(&HopInfo {
            limit: NonZeroU8::new(64).unwrap(),
            count: 1,
        })
        .0;

        let mut pack = FilterPack::new("test");
        pack.ingress_classifier(
            "reads",
            ReadsBlock {
                block: hop_count,
                expected: plaintext.clone(),
            },
        );
        let chains = freeze(pack);

        let keys = CountingKeys(AtomicUsize::new(0));
        parsed
            .decrypted
            .plaintext
            .insert(hop_count, plaintext.clone().into_boxed_slice().into());
        let ControlFlow::Continue(()) =
            chains.run_ingress(&mut parsed, &mut metadata, &keys, &mut HashMap::new())
        else {
            panic!("a passing Classifier must continue");
        };

        assert_eq!(
            keys.0.load(Ordering::Relaxed),
            0,
            "a block the header pass decrypted is not decrypted again"
        );
        assert_eq!(
            parsed
                .decrypted
                .plaintext
                .get(&hop_count)
                .map(|p| p.as_ref()),
            Some(plaintext.as_slice()),
            "the chain hands the cache back"
        );
    }
}
