use crate::{flags, io, keys};
use bytes::Bytes;
use clap::{Parser, ValueEnum};
use hardy_bpv7::{
    Bundle, CaptureFieldErr,
    block::{Block, Type},
    bpsec::{
        self, Role, Roles, SecurityService, SecurityServices, bib,
        checks::{self, Checked},
        edit::BPSecEditor,
        key::KeySet,
    },
    bundle_age::BundleAge,
    editor::{self, Chunk, Editor},
    eid::Eid,
    hop_info::HopInfo,
    parse,
};
use hardy_cbor::{decode::FromCbor, encode::emit};
use std::collections::{HashMap, HashSet};
/// Structural parse + keyed BPSec checks in one pass: each tool runs the
/// stages it needs and gets back a `parse::Parsed` ready to feed to
/// Editor / Signer / Encryptor — no second `parse::parse` call.
///
/// Stages run:
/// * structural parse (`parse::parse`);
/// * §A — the parser's classification, whose deletion verdict, `Error::Unsupported(n)`
///   (unknown block) or the error of a security block's unsupported
///   operation where a block flagged `delete_bundle_on_failure` can't be
///   processed, is the error;
/// * the receiver's keyed pass (`receive_with_keys`) with the supplied
///   keys: every plaintext BIB, and an encrypted BIB only where a per-hop
///   block needs it. A missing key is a soft skip; a failed decryption or
///   integrity check is rejected (tools are not Verifiers and do not apply
///   the §5.1.1 failure-drop).
///
/// An `Editor` over the result resolves a block whose coverage reads
/// `Maybe` with the same keys (`with_key_source`); the checking commands
/// read the bundle as its destination would through [`accepted_copy`].
pub(crate) fn parse_with_keys(
    data: Bytes,
    keys: &KeySet,
) -> Result<parse::Parsed, hardy_bpv7::Error> {
    let mut parsed = parse::parse(data)?;

    // §A — the parser's classification: a block demanding the bundle's
    // deletion is the error; the report-flag facts are ignored at this layer
    // (tools don't emit status reports).
    if let Some(error) = parsed.classification.delete_bundle.take() {
        return Err(error);
    }

    let facts = receive_with_keys(&mut parsed, keys)?;
    refuse_conflicting(&facts)?;
    if failed(&facts, SecurityService::Confidentiality) {
        return Err(bpsec::Error::DecryptionFailed.into());
    }
    if failed(&facts, SecurityService::Integrity) {
        return Err(bpsec::Error::IntegrityCheckFailed.into());
    }
    assert_nothing_deferred(&facts, &parsed.bundle);

    Ok(parsed)
}

/// The keyed pass a receiving node makes over a bundle, as the tool reads
/// one (`Parsed::check`): the per-hop blocks, every Previous Node, Bundle
/// Age and Hop Count block (RFC 9171 §5.4), accepted, both services, and
/// every other readable BIB operation verified, one `Facts`.
pub(crate) fn receive_with_keys(
    parsed: &mut parse::Parsed,
    keys: &KeySet,
) -> Result<checks::Facts, hardy_bpv7::Error> {
    let per_hop: HashMap<u64, Roles> = parsed
        .bundle
        .blocks
        .iter()
        .filter(|(_, b)| {
            matches!(
                b.block_type,
                Type::PreviousNode | Type::BundleAge | Type::HopCount
            )
        })
        .map(|(&n, _)| (n, Roles::each(Role::Acceptor, SecurityServices::BOTH)))
        .collect();
    parsed.check(
        keys,
        &per_hop,
        Roles::each(Role::Verifier, SecurityServices::INTEGRITY),
    )
}

/// The tool is strict: an integrity operation on a per-hop block inside an
/// encrypted BIB, proven or possible, is refused.
pub(crate) fn refuse_conflicting(facts: &checks::Facts) -> Result<(), hardy_bpv7::Error> {
    match facts
        .targets(Checked::Conflicting)
        .chain(facts.targets(Checked::PossiblyConflicting))
        .next()
    {
        Some(target) => Err(bpsec::Error::EncryptedBibCoversPerHopBlock(target).into()),
        None => Ok(()),
    }
}

/// Whether any operation of `service` failed its check.
pub(crate) fn failed(facts: &checks::Facts, service: SecurityService) -> bool {
    facts.targets_of(service, Checked::Failed).next().is_some()
}

/// Hard assert: this is shipped code, and a silently dropped deferral is an
/// unverified integrity statement. The tool parses complete buffers, so a
/// check defers nothing but the operations on an encrypted payload, which
/// [`accepted_copy`] decrypts for the checking commands to verify.
pub(crate) fn assert_nothing_deferred(facts: &checks::Facts, bundle: &Bundle) {
    let encrypted_payload = bundle.blocks.get(&1).is_some_and(|b| b.bcb.is_some());
    assert!(
        encrypted_payload || !facts.any(Checked::Deferred),
        "a complete buffer defers nothing"
    );
}

/// The bundle as its destination reads it: a copy with the encryption of
/// every block the keys can decrypt removed, as
/// `BPSecEditor::remove_encryption` removes it (with the encrypted BIB
/// coupled to the block), re-parsed so every BIB it decrypted is plaintext.
/// The encrypted BIBs go first, each with the targets coupled to it, since
/// the parse reads a Previous Node or Bundle Age block, which no valid
/// bundle has under an encrypted BIB, as covered by none. A block whose
/// encryption needs a key the set lacks stays encrypted; any other failure
/// is an error. The copy keeps the original's block numbers,
/// so its coverage and BIB OperationSets describe the original's blocks.
pub(crate) fn accepted_copy(
    parsed: &parse::Parsed,
    keys: &KeySet,
) -> anyhow::Result<parse::Parsed> {
    // The encrypted BIBs first, each taking the targets coupled to it.
    let (bibs, targets): (Vec<u64>, Vec<u64>) = parsed
        .bundle
        .blocks
        .iter()
        .filter(|(_, block)| block.bcb.is_some())
        .map(|(&n, _)| n)
        .partition(|n| parsed.bundle.blocks[n].block_type == Type::BlockIntegrity);
    let mut editor = Editor::new(&parsed.bundle, &parsed.data).with_key_source(keys);
    for block_number in bibs.into_iter().chain(targets) {
        match editor.remove_encryption(block_number) {
            // Taken with an earlier block's coupled BIB, or kept for want of
            // a key.
            Ok(())
            | Err(editor::Error::NotEncrypted | editor::Error::Bpsec(bpsec::Error::NoKey)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let chunks = editor.rebuild()?;
    Ok(parse::parse(Chunk::flatten_bytes(
        chunks,
        parsed.data.clone(),
    ))?)
}

/// CBOR-decode `T` from `data`, requiring it to consume the whole slice
/// (rejects trailing garbage). Shared by the per-command extension-block
/// helpers in `validate` / `inspect` / `full_rewrite`.
pub(crate) fn parse_exact<T>(data: &[u8], field: &'static str) -> Result<T, hardy_bpv7::Error>
where
    T: FromCbor,
    T::Error: From<hardy_cbor::decode::Error>,
    hardy_bpv7::Error: From<T::Error>,
{
    hardy_cbor::decode::parse_exact::<T>(data).map_field_err(field)
}

/// Decode a known plaintext extension block's body into `T`, tagging a parse
/// failure with `field`. Returns `None` for a BCB-encrypted block or one whose
/// payload isn't resident — the shared `bcb`-skip + `payload()` + field-tagged
/// `parse_exact` behind the `inspect` / `validate` / `full_rewrite` extraction
/// loops.
pub(crate) fn extract_known<T>(
    block: &Block,
    data: &[u8],
    field: &'static str,
) -> Result<Option<T>, hardy_bpv7::Error>
where
    T: FromCbor,
    T::Error: From<hardy_cbor::decode::Error>,
    hardy_bpv7::Error: From<T::Error>,
{
    if block.bcb.is_some() {
        return Ok(None);
    }
    block
        .payload(data)
        .map(|body| parse_exact::<T>(body, field))
        .transpose()
}

/// Verify the BIB signature over `block_number` using the BIB
/// OperationSets of an [`accepted_copy`], in which every BIB the keys
/// decrypt is plaintext. Returns `Ok(true)`
/// when the block is BIB-covered and the signature verified, `Ok(false)`
/// when the block has no BIB, and `Err(_)` for any verification error
/// (including `NoKey` — pass it through so the inspector can render the
/// per-block status).
pub(crate) fn verify_block(
    block_number: u64,
    blocks: &HashMap<u64, Block>,
    data: &[u8],
    bib_ops: &HashMap<u64, bib::OperationSet>,
    keys: &KeySet,
) -> Result<bool, hardy_bpv7::Error> {
    let target = blocks
        .get(&block_number)
        .ok_or(hardy_bpv7::Error::MissingBlock(block_number))?;

    let bib_block_number = match target.bib {
        hardy_bpv7::block::BibCoverage::Some(n) => n,
        hardy_bpv7::block::BibCoverage::None => return Ok(false),
        // Every BIB the keys decrypt is plaintext in an accepted copy, so a
        // coverage still unresolved is a BIB the keys cannot decrypt.
        hardy_bpv7::block::BibCoverage::Maybe => {
            return Err(hardy_bpv7::Error::InvalidBPSec(
                hardy_bpv7::bpsec::Error::NoKey,
            ));
        }
    };

    let opset = bib_ops
        .get(&bib_block_number)
        .ok_or(hardy_bpv7::Error::Altered)?;
    let op = opset
        .operations()
        .get(&block_number)
        .ok_or(hardy_bpv7::Error::Altered)?;
    let block_set = hardy_bpv7::reader::PlainReader {
        blocks,
        source_data: data,
    };
    op.verify(
        keys,
        bib::OperationArgs {
            source: opset.source(),
            target: block_number,
            security_block: bib_block_number,
            blocks: &block_set,
        },
    )
    .map(|_| true)
    .map_err(hardy_bpv7::Error::InvalidBPSec)
}

/// Full-mode parse + rewrite, returning the post-rewrite chunk plan
/// (or `None` if the bundle was already canonical and no blocks needed
/// removing). Composes the per-section helpers the same way the BPA
/// pipeline does.
///
/// NoKey policy mirrors Full mode: fatal for `HopCount` and unclocked
/// `BundleAge` extension blocks under BCB; soft for `PreviousNode` and
/// clocked `BundleAge`.
pub(crate) fn full_rewrite(data: Bytes, keys: &KeySet) -> anyhow::Result<Option<Vec<Chunk>>> {
    let mut parsed = parse::parse(data)?;

    // §A — the parser's classification; collect deletables.
    if let Some(error) = parsed.classification.delete_bundle.take() {
        return Err(error.into());
    }
    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(parsed.classification.unrecognised_deletable.iter().copied());
    to_remove.extend(parsed.classification.bib_deletable.iter().copied());

    // The receiver's keyed pass; the tool is strict on a Hop Count it has
    // no key for, and rejects any failed integrity check. The removal
    // cascade decrypts and checks the encrypted BIBs it passes through.
    let mut to_update: HashMap<u64, Vec<u8>> = HashMap::new();
    let facts = receive_with_keys(&mut parsed, keys)?;
    let parse::Parsed { data, bundle, .. } = parsed;
    refuse_conflicting(&facts)?;
    // RFC 9172 §5.1.1: a corrupt target is removed. The editor cascade
    // strips it from its covering BCB and drops the BCB once it empties;
    // naming the BCB here would strand a surviving co-target's ciphertext.
    to_remove.extend(facts.targets_of(SecurityService::Confidentiality, Checked::Failed));
    // A failed integrity check rejects the bundle.
    if failed(&facts, SecurityService::Integrity) {
        return Err(hardy_bpv7::bpsec::Error::IntegrityCheckFailed.into());
    }
    if facts.outcomes.iter().any(|o| {
        o.checked == Checked::NoKey
            && o.service == SecurityService::Confidentiality
            && bundle
                .blocks
                .get(&o.target)
                .is_some_and(|b| b.block_type == Type::HopCount)
    }) {
        return Err(hardy_bpv7::bpsec::Error::NoKey.into());
    }

    // §D — canonicalize known plaintext extension blocks. A non-shortest
    // PreviousNode/HopCount body is re-emitted in canonical form; an
    // encrypted body can't be re-emitted without re-encryption, so skip
    // it (`b.bcb.is_some()`). BundleAge is always canonical — decoded
    // here only to reject a malformed body.
    for (&n, b) in &bundle.blocks {
        match b.block_type {
            Type::PreviousNode => {
                if let Some((v, shortest)) =
                    extract_known::<(Eid, bool)>(b, &data, "Previous Node Block")?
                    && !shortest
                {
                    to_update.insert(n, emit(&v).0);
                }
            }
            Type::BundleAge => {
                extract_known::<BundleAge>(b, &data, "Bundle Age Block")?;
            }
            Type::HopCount => {
                if let Some((v, shortest)) =
                    extract_known::<(HopInfo, bool)>(b, &data, "Hop Count Block")?
                    && !shortest
                {
                    to_update.insert(n, emit(&v).0);
                }
            }
            _ => {}
        }
    }

    if to_update.is_empty() && to_remove.is_empty() {
        return Ok(None);
    }

    // §E — apply rewrites; discard the post-rewrite Bundle (tool only
    // needs the chunks for the wire-form output).
    Ok(hardy_bpv7::rewrite::apply_rewrites(
        Editor::new(&bundle, &data).with_key_source(keys),
        to_update,
        to_remove,
    )?
    .map(|(_, chunks)| chunks))
}

pub mod add_block;
pub mod compare;
pub mod create;
pub mod encrypt;
pub mod extract;
pub mod inspect;
pub mod remove_block;
pub mod remove_encryption;
pub mod remove_integrity;
pub mod rewrite;
pub mod sign;
pub mod update_block;
pub mod update_primary;
pub mod validate;
pub mod verify;
