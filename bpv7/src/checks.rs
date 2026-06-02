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
use error::HasInvalidField;
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
}

fn parse_exact<T>(data: &[u8], field: &'static str) -> Result<T, Error>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<hardy_cbor::decode::Error> + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    match hardy_cbor::decode::parse::<(T, usize)>(data) {
        Err(e) => Err(Error::invalid_field(field, e.into())),
        Ok((_, len)) if len != data.len() => {
            Err(Error::invalid_field(field, Error::AdditionalData.into()))
        }
        Ok((t, _)) => Ok(t),
    }
}

/// Output of [`classify_unrecognised_blocks`].
#[derive(Debug, Default)]
pub struct UnrecognisedClassification {
    /// Block numbers whose flags request `delete_block_on_failure`.
    /// Caller decides whether to honour the request.
    pub deletable: SmallVec<[u64; 4]>,
    /// At least one unrecognised block had `report_on_failure` set.
    pub report_unsupported: bool,
}

/// Output of [`classify_unsupported_bcbs`].
#[derive(Debug, Default)]
pub struct UnsupportedBcbClassification {
    /// At least one unsupported-context BCB had `report_on_failure` set.
    pub report_unsupported: bool,
}

/// Output of [`classify_unsupported_bibs`].
#[derive(Debug, Default)]
pub struct UnsupportedBibClassification {
    /// Plaintext BIB block numbers with `delete_block_on_failure`.
    /// Caller decides whether to honour: removing each from its own
    /// `bib_ops` map and scheduling the block for removal.
    pub deletable: SmallVec<[u64; 4]>,
    /// At least one unsupported-context BIB had `report_on_failure` set.
    pub report_unsupported: bool,
}

// ===== Section A — unrecognised / unsupported classification =====

/// A1: Scan `Type::Unrecognised` blocks. Per-flag classification only.
/// Returns `Err(Error::Unsupported(n))` if any block has
/// `delete_bundle_on_failure`.
///
/// `supported` lists block-type codes the caller actually understands
/// (e.g. extension types it has registered handlers for). A
/// `Type::Unrecognised(t)` block whose `t` is in `supported` is treated
/// as recognised — it never counts as unsupported and so never triggers
/// the delete/report/error handling below. Pass `&[]` when the caller
/// supports no extension types beyond the bpv7 built-ins.
pub fn classify_unrecognised_blocks(
    blocks: &HashMap<u64, block::Block>,
    supported: &[u64],
) -> Result<UnrecognisedClassification, Error> {
    let mut out = UnrecognisedClassification::default();
    for (&block_number, block) in blocks {
        let block::Type::Unrecognised(block_type) = block.block_type else {
            continue;
        };
        // The caller understands this block type, so it isn't "unsupported".
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
            out.deletable.push(block_number);
        }
    }
    Ok(out)
}

/// A2: Scan BCBs with unrecognised security contexts. Returns
/// `Err(Error::Unsupported(n))` if any such BCB has
/// `delete_bundle_on_failure`.
pub fn classify_unsupported_bcbs(
    blocks: &HashMap<u64, block::Block>,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
) -> Result<UnsupportedBcbClassification, Error> {
    let mut out = UnsupportedBcbClassification::default();
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
    Ok(out)
}

/// A3: Scan plaintext BIBs with unrecognised security contexts. Pure
/// classification — caller is responsible for removing the listed
/// `deletable` entries from its own `bib_ops` map if it intends to
/// honour the deletes. Encrypted BIBs (those whose body is BCB-protected)
/// are not visible here; they surface in Section B after decryption.
/// Returns `Err(Error::Unsupported(n))` if any such BIB has
/// `delete_bundle_on_failure`.
pub fn classify_unsupported_bibs(
    blocks: &HashMap<u64, block::Block>,
    bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
) -> Result<UnsupportedBibClassification, Error> {
    let mut out = UnsupportedBibClassification::default();
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
            out.deletable.push(bib_block_number);
        }
    }
    Ok(out)
}

// ===== Section B — decrypt-and-validate BCB-encrypted BIBs =====

/// B: For every BCB-encrypted BIB, decrypt the body, parse the
/// `bib::OperationSet`, run §3.8 / §3.9 structural checks, stamp BIB
/// coverage on every target, stash the plaintext into `decrypted_data`,
/// and insert the freshly-decoded OperationSet into `bib_ops`.
///
/// Returns `true` iff every encrypted BIB decrypted. When the caller
/// receives `true` it should invoke [`resolve_bib_coverage_maybes`] to
/// collapse residual `BibCoverage::Maybe` markers. On `false`, the eager
/// `Maybe` markers stamped by the parser stay put — an undecrypted BIB
/// might still claim them as targets.
///
/// NoKey on any individual BIB is soft (loop continues, returns `false`
/// at end). Any non-NoKey BPSec error fails the whole call.
pub fn decrypt_and_validate_covered_bibs(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    blocks: &mut HashMap<u64, block::Block>,
    bcb_ops: &HashMap<u64, bpsec::bcb::OperationSet>,
    bib_ops: &mut HashMap<u64, bpsec::bib::OperationSet>,
    decrypted_data: &mut HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    to_update: &HashMap<u64, Vec<u8>>,
) -> Result<bool, Error> {
    let mut all_decrypted = true;

    // Snapshot the encrypted-BIB block-number list up front so the loop
    // body can mutate `blocks` (coverage stamping) without conflicting
    // with the iteration.
    let encrypted_bibs: Vec<u64> = blocks
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
                    all_decrypted = false;
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
        // `check_bib` is trivially satisfied here because we only reach
        // this branch for BCB-encrypted BIBs.
        check_bib(&bib_op_set, bib_block_number, blocks)?;

        // §3.8 BCB-shares-target-with-BIB. Only fires when the BCB
        // context permits sharing — BCB-AES-GCM cannot (IV uniqueness).
        if bcb_op_set.can_share()
            && !bib_op_set
                .operations
                .keys()
                .any(|t| bcb_op_set.operations.contains_key(t))
        {
            return Err(bpsec::Error::InvalidBCBTarget.into());
        }

        // Stamp coverage. The parser stamped these targets as Maybe
        // because of this encrypted BIB; now we know the actual targets.
        for &target_number in bib_op_set.operations.keys() {
            blocks
                .get_mut(&target_number)
                .expect("check_bib verified every target exists")
                .bib = block::BibCoverage::Some(bib_block_number);
        }

        decrypted_data.insert(bib_block_number, plaintext);
        bib_ops.insert(bib_block_number, bib_op_set);
    }

    Ok(all_decrypted)
}

/// §B6: Collapse residual `BibCoverage::Maybe` markers to `None`. Call
/// only when [`decrypt_and_validate_covered_bibs`] returned `true`; if
/// any encrypted BIB returned NoKey the `Maybe` markers must stay put.
pub fn resolve_bib_coverage_maybes(blocks: &mut HashMap<u64, block::Block>) {
    for block in blocks.values_mut() {
        if matches!(block.bib, block::BibCoverage::Maybe) {
            block.bib = block::BibCoverage::None;
        }
    }
}

// ===== Section C7 — verify all BIBs =====

/// C7: Verify every BIB OperationSet against its targets. NoKey on
/// verify is a policy skip (matches BIB-decrypt-NoKey semantics); any
/// other verify error fails. Targets that are BCB-encrypted but absent
/// from `decrypted_data` (typically the payload) are skipped — the BPA
/// layer can re-verify with its own policy.
pub fn verify_all_bibs(
    data: &[u8],
    key_source: &dyn bpsec::key::KeySource,
    blocks: &HashMap<u64, block::Block>,
    bib_ops: &HashMap<u64, bpsec::bib::OperationSet>,
    decrypted_data: &HashMap<u64, zeroize::Zeroizing<Box<[u8]>>>,
    to_update: &HashMap<u64, Vec<u8>>,
) -> Result<(), Error> {
    for (&bib_block_number, ops) in bib_ops {
        for (&target_number, op) in &ops.operations {
            let target_block = blocks
                .get(&target_number)
                .expect("check_bib verified targets exist");
            if target_block.bcb.is_some() && !decrypted_data.contains_key(&target_number) {
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
}

/// Composed keyed verification, replacing the hand-rolled
/// B → resolve → C8 → C7 pipeline that every keyed call site used to
/// repeat:
/// * §B — [`decrypt_and_validate_covered_bibs`], collapsing residual
///   `Maybe` coverage via [`resolve_bib_coverage_maybes`] when every
///   covered BIB decrypted;
/// * §C8 — decrypt BCB-protected `PreviousNode` / `BundleAge` /
///   `HopCount`, recording per-block `Decrypted`/`NoKey`/`Failed`
///   outcomes;
/// * §C7 — [`verify_all_bibs`].
///
/// Successful decrypts (covered BIBs and extension blocks) are stashed
/// into `decrypted`; BIB coverage is stamped on `blocks`. Returns the
/// [`VerifyFacts`] the caller applies its policy to. A non-`NoKey` /
/// non-`DecryptionFailed` BPSec error, or a BIB-verify failure, fails the
/// whole call.
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
    // decrypted.
    if decrypt_and_validate_covered_bibs(
        data, key_source, blocks, bcb_ops, bib_ops, decrypted, to_update,
    )? {
        resolve_bib_coverage_maybes(blocks);
    }

    // §C8 — decrypt BCB-protected extension blocks.
    let to_decrypt: Vec<(u64, block::Type, u64)> = blocks
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

    // §C7 — verify every BIB.
    verify_all_bibs(data, key_source, blocks, bib_ops, decrypted, to_update)?;

    Ok(facts)
}

// ===== Per-OperationSet structural checks (shared with the parser) =====

/// Per-OperationSet structural validation of a BIB against a parsed
/// `Bundle`. Checks the rules that depend only on this one BIB's
/// targets — every target block number exists in the bundle (§3.6)
/// and no target is a security block (§3.9: BIB MUST NOT target a
/// BCB, mirrored to also reject BIB-targets-BIB).
///
/// Does NOT check rules that require seeing every BIB at once (the
/// §2.6 "security operations unique per (service, target)" rule —
/// i.e. no duplicate targets across BIBs); that orchestration stays
/// at the caller, which holds the running cross-BIB target map.
///
/// Pure inspection: no mutation of `bundle`, no stamping of `bib`
/// coverage. The caller stamps after a successful return.
///
/// Intended to migrate to a method on `bpsec::bib::OperationSet` once
/// the BIB OperationSet API stabilises. The structural [`crate::parse`]
/// pass and the keyed BPSec filter both call this, so it is the single
/// source of truth for the per-OperationSet rules.
///
/// Note: RFC 9172 §3.8 (BCB-targeting-BIB must share a target with
/// the BIB) is *not* checked here — that rule fires only for
/// BCB-encrypted BIBs whose OperationSet we cannot decode without
/// keys. See `bpv7/docs/TODO.md` "Keyed BPSec filter: RFC 9172 §3.8".
pub fn check_bib(
    ops: &bpsec::bib::OperationSet,
    bib_block_number: u64,
    blocks: &HashMap<u64, block::Block>,
) -> Result<(), Error> {
    // Whether this BIB is itself protected by a BCB — load once, used
    // by the §3.9 check on each target.
    let bib_bcb = blocks
        .get(&bib_block_number)
        .expect("check_bib called with a bib_block_number not in blocks")
        .bcb;

    for &target_number in ops.operations.keys() {
        let target_block = blocks
            .get(&target_number)
            .ok_or(bpsec::Error::MissingSecurityTarget)?;
        if matches!(
            target_block.block_type,
            block::Type::BlockSecurity | block::Type::BlockIntegrity
        ) {
            return Err(bpsec::Error::InvalidBIBTarget.into());
        }
        if matches!(target_block.bib, block::BibCoverage::Some(n) if n != bib_block_number) {
            return Err(bpsec::Error::DuplicateOpTarget.into());
        }
        // RFC 9172 §3.9: a BIB whose target block is encrypted with a
        // BCB MUST itself be encrypted with a BCB. (The stricter
        // "same BCB or a superset BCB" rule requires decrypting both;
        // we mirror the legacy parser's "any BCB" check here.)
        if target_block.bcb.is_some() && bib_bcb.is_none() {
            return Err(bpsec::Error::BIBMustBeEncrypted.into());
        }
    }
    Ok(())
}

/// Per-OperationSet structural validation of a BCB against a parsed
/// `Bundle`. Checks the rules that depend only on this one BCB's flags
/// and targets — the BCB MUST NOT set delete-block-on-failure (§3.7),
/// every target block must exist (§3.6), the BCB MUST NOT target the
/// primary block or another BCB (§3.7), and if the BCB targets the
/// payload it MUST set must-replicate (§3.7).
///
/// Does NOT check rules that require seeing every BCB at once (the
/// §2.6 "security operations unique per (service, target)" rule —
/// i.e. no duplicate targets across BCBs); that orchestration stays
/// at the caller, which holds the running cross-BCB target map.
///
/// Pure inspection: no mutation of `bundle`, no stamping of `bcb`
/// coverage. The caller stamps after a successful return.
///
/// Intended to migrate to a method on `bpsec::bcb::OperationSet` once
/// the BCB OperationSet API stabilises. The structural [`crate::parse`]
/// pass and the keyed BPSec filter both call this, so it is the single
/// source of truth for the per-OperationSet BCB rules.
pub fn check_bcb(
    ops: &bpsec::bcb::OperationSet,
    bcb_block_number: u64,
    bcb_flags: &block::Flags,
    blocks: &HashMap<u64, block::Block>,
) -> Result<(), Error> {
    if bcb_flags.delete_block_on_failure {
        return Err(bpsec::Error::BCBDeleteFlag.into());
    }
    for &target_number in ops.operations.keys() {
        let target_block = blocks
            .get(&target_number)
            .ok_or(bpsec::Error::MissingSecurityTarget)?;
        match target_block.block_type {
            block::Type::Primary | block::Type::BlockSecurity => {
                return Err(bpsec::Error::InvalidBCBTarget.into());
            }
            block::Type::Payload if !bcb_flags.must_replicate => {
                return Err(bpsec::Error::BCBMustReplicate.into());
            }
            _ => {}
        }
        if matches!(target_block.bcb, Some(n) if n != bcb_block_number) {
            return Err(bpsec::Error::DuplicateOpTarget.into());
        }
    }
    Ok(())
}
