/*!
Composable BPSec validation primitives over a structurally-parsed bundle.

Each helper is policy-free: it produces facts (classifications,
decrypt/verify outcomes, coverage stamps) and applies no policy of its
own. Consumers — the BPA ingress pipeline, the bpv7 CLI tools — compose
the helpers they need (or call [`verify`] for the whole keyed pass) and
layer their own policy on top. The structural parser lives in
[`crate::parse`]; rewrite application in [`crate::rewrite`].

The §A–§E pipeline these helpers implement — what each `A1` / `A2` /
`A3` / `B` / `B6` / `C7` / `C8` / `D` / `E` label means — is documented
in `bpv7/docs/parser_design.md`.
*/

use super::*;
use error::CaptureFieldErr;
use smallvec::SmallVec;

/// View into a partially-processed bundle for BPSec operations.
///
/// Returns the current best payload for each block: a decrypted body if a
/// BCB target has been decrypted, an in-progress canonical rewrite if its
/// OperationSet was shrunk, or the original byte range from `source_data`.
/// Takes `&HashMap<u64, block::Block>` directly — no Bundle type
/// dependency.
struct BundleBlockSet<'a> {
    blocks: &'a HashMap<u64, block::Block>,
    source_data: &'a [u8],
    decrypted_data: &'a HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    to_update: &'a HashMap<u64, Vec<u8>>,
}

impl<'a> bpsec::BlockSet<'a> for BundleBlockSet<'a> {
    fn block(
        &'a self,
        block_number: u64,
    ) -> Option<(&'a block::Block, Option<block::Payload<'a>>)> {
        let block = self.blocks.get(&block_number)?;
        let payload = if let Some(b) = self.decrypted_data.get(&block_number) {
            Some(b.as_ref())
        } else if let Some(b) = self.to_update.get(&block_number) {
            Some(b.as_slice())
        } else {
            // `source_data` is the full in-memory bundle.
            block.payload(self.source_data)
        };
        Some((block, payload.map(block::Payload::Borrowed)))
    }

    fn block_header(&'a self, block_number: u64) -> Option<&'a block::Block> {
        self.blocks.get(&block_number)
    }
}

fn parse_exact<T>(data: &[u8], field: &'static str) -> Result<T, Error>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<hardy_cbor::decode::Error> + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    hardy_cbor::decode::parse_exact::<T>(data).map_field_err(field)
}

/// Output of [`classify_unsupported`]: the blocks this node can't process,
/// classified by what the caller may do about them.
#[derive(Debug, Default)]
pub struct Classification {
    /// Unrecognised blocks flagged `delete_block_on_failure`. The caller
    /// decides whether to honour the request (schedule them for removal).
    pub unrecognised_deletable: SmallVec<[u64; 4]>,
    /// Plaintext BIB blocks with an unrecognised security context, flagged
    /// `delete_block_on_failure`. The caller decides whether to honour:
    /// removing each from its own `bib_ops` map and scheduling the block
    /// for removal.
    pub bib_deletable: SmallVec<[u64; 4]>,
    /// At least one unrecognised block or unsupported-context BCB/BIB had
    /// `report_on_failure` set.
    pub report_unsupported: bool,
}

// ===== Section A — unrecognised / unsupported classification =====

/// §A: Classify the blocks this node can't process — `Type::Unrecognised`
/// blocks (A1), BCBs with an unrecognised security context (A2), and
/// plaintext BIBs with an unrecognised security context (A3) — into the
/// per-flag [`Classification`] facts. Returns `Err(Error::Unsupported(n))`
/// if any such block sets `delete_bundle_on_failure`.
///
/// `supported` lists block-type codes the caller actually understands
/// (e.g. extension types it has registered handlers for); a
/// `Type::Unrecognised(t)` block whose `t` is in `supported` is treated as
/// recognised and ignored. Pass `&[]` when the caller supports no
/// extension types beyond the bpv7 built-ins.
///
/// BCB-encrypted BIBs aren't visible here — their bodies are ciphertext;
/// they surface in §B after decryption. Pure classification: the caller
/// decides whether to honour the `*_deletable` lists.
pub fn classify_unsupported(
    blocks: &HashMap<u64, block::Block>,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
    supported: &[u64],
) -> Result<Classification, Error> {
    let mut out = Classification::default();

    // A1 — unrecognised blocks.
    for (&block_number, block) in blocks {
        let block::Type::Unrecognised(block_type) = block.block_type else {
            continue;
        };
        if supported.contains(&block_type) {
            continue;
        }
        if block.flags.delete_bundle_on_failure {
            return Err(Error::Unsupported(block_number));
        }
        if block.flags.report_on_failure {
            out.report_unsupported = true;
        }
        if block.flags.delete_block_on_failure {
            out.unrecognised_deletable.push(block_number);
        }
    }

    // A2 — BCBs with an unrecognised security context.
    for (&bcb_block_number, ops) in bcb_ops {
        if !ops.is_unsupported() {
            continue;
        }
        let flags = &blocks
            .get(&bcb_block_number)
            .expect("BCB number from bcb_ops must exist in blocks")
            .flags;
        if flags.delete_bundle_on_failure {
            return Err(Error::Unsupported(bcb_block_number));
        }
        if flags.report_on_failure {
            out.report_unsupported = true;
        }
    }

    // A3 — plaintext BIBs with an unrecognised security context.
    for (&bib_block_number, ops) in bib_ops {
        if !ops.is_unsupported() {
            continue;
        }
        let flags = &blocks
            .get(&bib_block_number)
            .expect("BIB number from bib_ops must exist in blocks")
            .flags;
        if flags.delete_bundle_on_failure {
            return Err(Error::Unsupported(bib_block_number));
        }
        if flags.report_on_failure {
            out.report_unsupported = true;
        }
        if flags.delete_block_on_failure {
            out.bib_deletable.push(bib_block_number);
        }
    }

    Ok(out)
}

// ===== Section B — decrypt-and-validate BCB-encrypted BIBs =====

/// §B: For every BCB-encrypted BIB, decrypt the body, parse the
/// `bib::OperationSet`, run §3.8 / §3.9 structural checks, stamp BIB
/// coverage on every target, stash the plaintext into `decrypted_data`,
/// and insert the freshly-decoded OperationSet into `bib_ops`.
///
/// Returns the block numbers of any BIBs that failed to decrypt
/// (`DecryptionFailed` — ciphertext corrupt, RFC 9172 §5.1.1). The
/// caller applies the failure policy: for a Verifier, failure-drop
/// (schedule for removal); for a strict acceptor, reject the bundle.
///
/// `NoKey` on any individual BIB is a soft skip (loop continues; that
/// BIB's `BibCoverage::Maybe` markers are left in place). Any non-NoKey,
/// non-`DecryptionFailed` BPSec error fails the whole call. When all
/// covered BIBs decrypted with no failures, residual `BibCoverage::Maybe`
/// markers are collapsed to `None` before returning.
pub fn decrypt_and_validate_covered_bibs(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    blocks: &mut HashMap<u64, block::Block>,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &mut HashMap<u64, bpsec::bib::OperationSet>,
    decrypted_data: &mut HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    to_update: &HashMap<u64, Vec<u8>>,
) -> Result<SmallVec<[u64; 4]>, Error> {
    let mut had_nokey = false;
    let mut failed: SmallVec<[u64; 4]> = SmallVec::new();

    // Snapshot the encrypted-BIB block-number list up front so the loop
    // body can mutate `blocks` (coverage stamping) without conflicting
    // with the iteration.
    let encrypted_bibs: SmallVec<[u64; 4]> = blocks
        .iter()
        .filter_map(|(&n, b)| {
            (matches!(b.block_type, block::Type::BlockIntegrity) && b.bcb.is_some()).then_some(n)
        })
        .collect();

    for bib_block_number in encrypted_bibs {
        let bcb_block_number = blocks
            .get(&bib_block_number)
            .expect("encrypted BIB filtered above")
            .bcb
            .expect("encrypted BIB filtered above");

        let bcb_op_set = bcb_ops
            .get(&bcb_block_number)
            .expect("BCB referenced by an encrypted BIB must be in bcb_ops");

        // Scope the BlockSet so its `blocks` borrow ends before we mutate
        // for coverage stamping.
        let plaintext = {
            let block_set = BundleBlockSet {
                blocks,
                source_data: data,
                decrypted_data,
                to_update,
            };
            let bcb_op = bcb_op_set
                .operations
                .get(&bib_block_number)
                .expect("BCB-protected BIB must have a BCB op for itself");
            match bcb_op.decrypt(
                key_source,
                bpsec::bcb::OperationArgs {
                    bpsec_source: &bcb_op_set.source,
                    target: bib_block_number,
                    source: bcb_block_number,
                    blocks: &block_set,
                },
            ) {
                Ok(p) => p,
                Err(bpsec::Error::NoKey) => {
                    had_nokey = true;
                    continue;
                }
                Err(bpsec::Error::DecryptionFailed) => {
                    // RFC 9172 §5.1.1: ciphertext corrupt — surface as a
                    // failed fact. Caller applies failure-drop policy.
                    // Coverage stamps are NOT applied (targets remain Maybe).
                    failed.push(bib_block_number);
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        };

        // `bib::OperationSet` is strict-canonical end-to-end, so the
        // shortest flag is always true and `parse_exact` handles the
        // trailing-garbage check.
        let bib_op_set =
            parse_exact::<bpsec::bib::OperationSet>(&plaintext, "BPSec integrity extension block")?;

        // Per-OperationSet structural rules — single source of truth
        // shared with the keyless parser. The §3.9 "if target is
        // BCB-encrypted then BIB must be BCB-encrypted" check inside
        // `OperationSet::check` is trivially satisfied here because we only
        // reach this branch for BCB-encrypted BIBs.
        bib_op_set.check(
            bib_block_number,
            &bpsec::PlainBlockSet {
                blocks: &*blocks,
                source_data: data,
            },
        )?;

        // §3.8 BCB-shares-target-with-BIB. Only fires when the BCB
        // context permits sharing — BCB-AES-GCM cannot (IV uniqueness).
        if bcb_op_set.can_share()
            && !bib_op_set
                .operations
                .keys()
                .any(|t| bcb_op_set.operations.contains_key(t))
        {
            return Err(bpsec::Error::BCBMustShareTarget.into());
        }

        // Stamp coverage. The parser stamped these targets as Maybe
        // because of this encrypted BIB; now we know the actual targets.
        for &target_number in bib_op_set.operations.keys() {
            blocks
                .get_mut(&target_number)
                .expect("OperationSet::check verified every target exists")
                .bib = block::BibCoverage::Some(bib_block_number);
        }

        decrypted_data.insert(bib_block_number, plaintext);
        bib_ops.insert(bib_block_number, bib_op_set);
    }

    // Collapse residual Maybe markers only when every covered BIB both
    // decrypted and authenticated. A NoKey BIB may still claim any Maybe
    // target; a failed BIB leaves its targets unknown until it is dropped.
    if !had_nokey && failed.is_empty() {
        for block in blocks.values_mut() {
            if matches!(block.bib, block::BibCoverage::Maybe) {
                block.bib = block::BibCoverage::None;
            }
        }
    }

    Ok(failed)
}

// ===== Section C7 — verify all BIBs =====

/// C7: Verify every BIB OperationSet against its targets, returning the BIB
/// block numbers whose op-set still has an **unchecked block-1 (payload)
/// target** — which happens when run on a headers-only buffer (the streaming
/// ingress gate), where the payload's over-claiming extent isn't resident so
/// its bytes can't be read yet. Those are the op-sets the caller must defer to
/// [`verify_payload`] once the payload is resident; the gate uses the returned
/// set to `retain` exactly them in its `bib_ops` map (the leftover map then *is*
/// the deferred set). For an all-resident buffer every target is checked and the
/// returned set is **empty** — nothing to defer.
///
/// `bib_ops` is **borrowed**, not drained: this stays a reusable verifier for
/// the all-resident callers (tools, tests) that still need the map afterwards.
/// Only the gate drains, and it does so itself from the returned set.
///
/// NoKey on verify is a policy skip (matches BIB-decrypt-NoKey semantics); any
/// other verify error fails. Targets that are BCB-encrypted but absent from
/// `decrypted_data` (a confidentiality-protected payload) are skipped and *not*
/// deferred — re-verified at delivery once decrypted.
pub fn verify_all_bibs(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    blocks: &HashMap<u64, block::Block>,
    bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
    decrypted_data: &HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    to_update: &HashMap<u64, Vec<u8>>,
) -> Result<SmallVec<[u64; 4]>, Error> {
    let mut deferred = SmallVec::new();
    for (&bib_block_number, ops) in bib_ops {
        let mut defer = false;
        for (&target_number, op) in &ops.operations {
            let target_block = blocks
                .get(&target_number)
                .expect("OperationSet::check verified targets exist");
            if target_block.bcb.is_some() && !decrypted_data.contains_key(&target_number) {
                continue;
            }
            // Target bytes not resident — the payload (block 1), whose
            // over-claiming extent isn't in a headers-only buffer. Defer this
            // op-set's block-1 target to `verify_payload` on the full bundle.
            // Never taken for an all-resident buffer.
            if !decrypted_data.contains_key(&target_number)
                && !to_update.contains_key(&target_number)
                && target_block.payload(data).is_none()
            {
                defer = true;
                continue;
            }
            let block_set = BundleBlockSet {
                blocks,
                source_data: data,
                decrypted_data,
                to_update,
            };
            match op.verify(
                key_source,
                bpsec::bib::OperationArgs {
                    bpsec_source: &ops.source,
                    target: target_number,
                    source: bib_block_number,
                    blocks: &block_set,
                },
            ) {
                Ok(()) => {}
                Err(bpsec::Error::NoKey) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if defer {
            deferred.push(bib_block_number);
        }
    }
    Ok(deferred)
}

/// Second-pass companion to [`verify_all_bibs`] for the streaming ingress
/// gate: verify the **block-1 (payload)** target of every BIB in `bib_ops`
/// against the now-resident full bundle `data`. The header pass
/// ([`verify`] on a headers-only buffer) skips block-1 targets because the
/// payload isn't yet resident, and hands the caller exactly the op-sets that
/// still target block 1; this re-checks only those targets (header targets
/// were already verified in the first pass — no re-checking).
///
/// `NoKey` is a soft skip, as in [`verify_all_bibs`]. A BCB-encrypted payload
/// is skipped here too — its integrity is established at delivery, when the
/// payload is decrypted ([`bpsec::block_data`]).
pub fn verify_payload(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    blocks: &HashMap<u64, block::Block>,
    bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
    decrypted_data: &HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    to_update: &HashMap<u64, Vec<u8>>,
) -> Result<(), Error> {
    for (&bib_block_number, ops) in bib_ops {
        let Some(op) = ops.operations.get(&1) else {
            continue;
        };
        let target_block = blocks.get(&1).expect("payload block exists");
        if target_block.bcb.is_some() && !decrypted_data.contains_key(&1) {
            continue;
        }
        let block_set = BundleBlockSet {
            blocks,
            source_data: data,
            decrypted_data,
            to_update,
        };
        match op.verify(
            key_source,
            bpsec::bib::OperationArgs {
                bpsec_source: &ops.source,
                target: 1,
                source: bib_block_number,
                blocks: &block_set,
            },
        ) {
            Ok(()) => {}
            Err(bpsec::Error::NoKey) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

// ===== Composed keyed verification (B + C8 + C7) =====

/// Per-operation outcomes from [`verify`] that the caller layers policy
/// on. Successful decrypts are stashed into `decrypted` and BIB coverage
/// is stamped on `blocks` in place during the call; these are the
/// outcomes that still need a policy decision.
#[derive(Debug, Default)]
pub struct VerifyFacts {
    /// BCB-protected extension blocks whose ciphertext did not
    /// authenticate (`DecryptionFailed`) — corruption (RFC 9172 §5.1.1).
    /// Caller applies the failure policy: failure-drop (Verifier/Acceptor)
    /// or reject.
    pub failed: SmallVec<[u64; 4]>,
    /// BCB-protected extension blocks for which no key was available, with
    /// their block type — caller applies the per-type NoKey policy
    /// (Preserve-soft; strict for `HopCount` + unclocked `BundleAge`).
    pub nokey_ext: SmallVec<[(u64, block::Type); 4]>,
    /// BIB OperationSets (by block number) with an unchecked block-1 (payload)
    /// target — the payload wasn't resident in this buffer (the streaming
    /// ingress gate ran on headers only). The gate retains exactly these in its
    /// `bib_ops` and re-verifies them via [`verify_payload`] once the payload is
    /// drained. Empty for an all-resident buffer.
    pub deferred_bibs: SmallVec<[u64; 4]>,
}

/// Composed keyed verification: §B → §C8 → §C7.
///
/// * §B — [`decrypt_and_validate_covered_bibs`]: decrypts and validates
///   BCB-covered BIBs, stamps BIB coverage, collapses residual `Maybe`
///   markers when every covered BIB decrypted with no failures.
/// * §C8 — decrypts BCB-protected `PreviousNode` / `BundleAge` /
///   `HopCount`, recording per-block outcomes.
/// * §C7 — [`verify_all_bibs`]: verifies all BIB OperationSets.
///
/// Successful decrypts are stashed into `decrypted`; BIB coverage is
/// stamped on `blocks`. Returns [`VerifyFacts`] — the caller applies its
/// policy to `failed` (corrupt blocks, RFC 9172 §5.1.1) and `nokey_ext`.
/// A non-`NoKey` / non-`DecryptionFailed` BPSec error, or a BIB-verify
/// failure, propagates as `Err`.
pub fn verify(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    blocks: &mut HashMap<u64, block::Block>,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &mut HashMap<u64, bpsec::bib::OperationSet>,
    decrypted: &mut HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    to_update: &HashMap<u64, Vec<u8>>,
) -> Result<VerifyFacts, Error> {
    let mut facts = VerifyFacts::default();

    // §B — decrypt + validate BCB-covered BIBs; resolve coverage if all
    // decrypted with no failures (done inside the call).
    let failed_bibs = decrypt_and_validate_covered_bibs(
        data, key_source, blocks, bcb_ops, bib_ops, decrypted, to_update,
    )?;
    facts.failed.extend(failed_bibs);

    // §C8 — decrypt BCB-protected extension blocks.
    let to_decrypt: SmallVec<[(u64, block::Type, u64); 4]> = blocks
        .iter()
        .filter_map(|(&n, b)| {
            matches!(
                b.block_type,
                block::Type::PreviousNode | block::Type::BundleAge | block::Type::HopCount
            )
            .then(|| b.bcb.map(|bcb_n| (n, b.block_type, bcb_n)))
            .flatten()
        })
        .collect();
    for (target_number, target_type, bcb_block_number) in to_decrypt {
        let bcb_op_set = bcb_ops
            .get(&bcb_block_number)
            .expect("BCB referenced by encrypted block must be in bcb_ops");
        let result = {
            let block_set = BundleBlockSet {
                blocks,
                source_data: data,
                decrypted_data: decrypted,
                to_update,
            };
            let bcb_op = bcb_op_set
                .operations
                .get(&target_number)
                .expect("BCB protects this target");
            bcb_op.decrypt(
                key_source,
                bpsec::bcb::OperationArgs {
                    bpsec_source: &bcb_op_set.source,
                    target: target_number,
                    source: bcb_block_number,
                    blocks: &block_set,
                },
            )
        };
        match result {
            Ok(p) => {
                decrypted.insert(target_number, p);
            }
            Err(bpsec::Error::NoKey) => facts.nokey_ext.push((target_number, target_type)),
            Err(bpsec::Error::DecryptionFailed) => facts.failed.push(target_number),
            Err(e) => return Err(e.into()),
        }
    }

    // §C7 — verify every BIB, recording the op-sets with a deferred block-1
    // (payload) target so the gate can retain exactly them. (A block-1 BCB —
    // payload confidentiality — is left untouched in `bcb_ops` by §B/§C8 and
    // decrypted at delivery via `bpsec::block_data`.)
    facts.deferred_bibs = verify_all_bibs(data, key_source, blocks, bib_ops, decrypted, to_update)?;

    Ok(facts)
}

// Per-OperationSet structural checks live as `check` methods on
// `bpsec::bib::OperationSet` / `bpsec::bcb::OperationSet`, the single
// source of truth shared with the structural parser.
