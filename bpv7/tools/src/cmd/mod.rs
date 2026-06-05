use crate::{flags, io, keys};
use bytes::Bytes;
use clap::{Parser, ValueEnum};
use hardy_bpv7::bpsec::{bib, key::KeySet};
use hardy_bpv7::{Bundle, checks, parse};
use std::collections::HashMap;

/// Structural parse + keyed BPSec validation in one pass: each tool runs
/// the stages it needs and gets back a `parse::Parsed` (already
/// coverage-stamped) ready to feed to Editor / Signer / Encryptor — no
/// second `parse::parse` call.
///
/// Stages run:
/// * structural parse (`parse::parse`);
/// * §A — `classify_unsupported`, which surfaces `Error::Unsupported(n)`
///   if a block flagged `delete_bundle_on_failure` is unknown/unsupported;
/// * §B — `decrypt_and_validate_covered_bibs` with the supplied keys
///   (`NoKey` is a soft skip; `DecryptionFailed` is rejected here — tools
///   are not Verifiers and do not apply §5.1.1 failure-drop);
/// * §C7 — `verify_all_bibs` with the supplied keys (`NoKey` is soft).
///
/// Returns a [`parse::Parsed`] with the bundle's block-coverage stamps
/// already updated by §B.
pub(crate) fn parse_with_keys(
    data: Bytes,
    keys: &KeySet,
) -> Result<parse::Parsed, hardy_bpv7::Error> {
    let mut parsed = parse::parse(data)?;

    // §A — classification. `?` propagates Unsupported on
    // delete_bundle_on_failure blocks; report-flag side effects are
    // ignored at this layer (tools don't emit status reports).
    checks::classify_unsupported(&parsed.bundle.blocks, &parsed.bcbs, &parsed.bibs, &[])?;

    // §B — decrypt + validate BCB-covered BIBs. NoKey is a soft skip;
    // DecryptionFailed surfaces in `failed_bibs` and is rejected here.
    let mut decrypted_data = HashMap::new();
    let no_updates = HashMap::new();
    let failed_bibs = checks::decrypt_and_validate_covered_bibs(
        &parsed.data,
        keys,
        &mut parsed.bundle.blocks,
        &parsed.bcbs,
        &mut parsed.bibs,
        &mut decrypted_data,
        &no_updates,
    )?;
    if !failed_bibs.is_empty() {
        return Err(hardy_bpv7::bpsec::Error::DecryptionFailed.into());
    }

    // §C7 — verify every BIB with the supplied keys. `verify_all_bibs` borrows
    // the op-map (the buffer is complete, so it defers nothing), leaving
    // `parsed.bibs` intact for the later per-block `verify_block`.
    checks::verify_all_bibs(
        &parsed.data,
        keys,
        &parsed.bundle.blocks,
        &parsed.bibs,
        &decrypted_data,
        &no_updates,
    )?;

    Ok(parsed)
}

/// CBOR-decode `T` from `data`, requiring it to consume the whole slice
/// (rejects trailing garbage). Shared by the per-command extension-block
/// helpers in `validate` / `inspect` / `full_rewrite`.
pub(crate) fn parse_exact<T>(data: &[u8], field: &'static str) -> Result<T, hardy_bpv7::Error>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<hardy_cbor::decode::Error> + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    match hardy_cbor::decode::parse::<(T, usize)>(data) {
        Err(e) => Err(hardy_bpv7::Error::InvalidField {
            field,
            source: e.into(),
        }),
        Ok((_, len)) if len != data.len() => Err(hardy_bpv7::Error::InvalidField {
            field,
            source: hardy_bpv7::Error::AdditionalData.into(),
        }),
        Ok((t, _)) => Ok(t),
    }
}

/// Verify the BIB signature over `block_number` using the BIB
/// OperationSets returned from [`parse_with_keys`] (which already
/// folded in any BCB-decrypted BIB bodies during §B). Returns `Ok(true)`
/// when the block is BIB-covered and the signature verified, `Ok(false)`
/// when the block has no BIB, and `Err(_)` for any verification error
/// (including `NoKey` — pass it through so the inspector can render the
/// per-block status).
pub(crate) fn verify_block(
    block_number: u64,
    blocks: &HashMap<u64, hardy_bpv7::block::Block>,
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
        hardy_bpv7::block::BibCoverage::Maybe => {
            return Err(hardy_bpv7::Error::InvalidBPSec(
                hardy_bpv7::bpsec::Error::MaybeHasBib(block_number),
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
    let block_set = hardy_bpv7::bpsec::PlainBlockSet {
        blocks,
        source_data: data,
    };
    op.verify(
        keys,
        bib::OperationArgs {
            bpsec_source: opset.source(),
            target: block_number,
            source: bib_block_number,
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
#[allow(clippy::result_large_err)]
pub(crate) fn full_rewrite(
    data: Bytes,
    keys: &KeySet,
) -> Result<Option<Vec<hardy_bpv7::editor::Chunk>>, hardy_bpv7::Error> {
    use std::collections::HashSet;

    let parse::Parsed {
        data,
        mut bundle,
        bcbs: bcb_ops,
        bibs: mut bib_ops,
    } = parse::parse(data)?;

    // §A — classify; collect deletables.
    let classification = checks::classify_unsupported(&bundle.blocks, &bcb_ops, &bib_ops, &[])?;
    let mut to_remove: HashSet<u64> = HashSet::new();
    to_remove.extend(classification.unrecognised_deletable);
    for n in &classification.bib_deletable {
        to_remove.insert(*n);
        bib_ops.remove(n);
    }

    // §B + §C8 + §C7 — composed keyed verification (strict NoKey for
    // HopCount + unclocked BundleAge; a §C8 decrypt failure is rejected).
    let mut decrypted = HashMap::new();
    let mut to_update: HashMap<u64, Vec<u8>> = HashMap::new();
    let facts = checks::verify(
        &data,
        keys,
        &mut bundle.blocks,
        &bcb_ops,
        &mut bib_ops,
        &mut decrypted,
        &to_update,
    )?;
    // RFC 9172 §5.1.1: corrupt payload → discard bundle; corrupt
    // non-payload → remove the target and its security block.
    for &target in &facts.failed {
        if target == 1 {
            return Err(hardy_bpv7::bpsec::Error::DecryptionFailed.into());
        }
        to_remove.insert(target);
        if let Some(bcb) = bundle.blocks.get(&target).and_then(|b| b.bcb) {
            to_remove.insert(bcb);
        }
    }
    for (_, block_type) in &facts.nokey_ext {
        match block_type {
            hardy_bpv7::block::Type::HopCount => {
                return Err(hardy_bpv7::bpsec::Error::NoKey.into());
            }
            hardy_bpv7::block::Type::BundleAge if !bundle.primary.id.timestamp.is_clocked() => {
                return Err(hardy_bpv7::bpsec::Error::NoKey.into());
            }
            _ => {}
        }
    }

    // §D — canonicalize known plaintext extension blocks. A non-shortest
    // PreviousNode/HopCount body is re-emitted in canonical form; an
    // encrypted body can't be re-emitted without re-encryption, so skip
    // it (`b.bcb.is_some()`). BundleAge is always canonical — decoded
    // here only to reject a malformed body.
    for (&n, b) in &bundle.blocks {
        if b.bcb.is_some() {
            continue;
        }
        // `data` is the full in-memory bundle from `parse::parse`.
        let Some(body) = b.payload(&data) else {
            continue;
        };
        match b.block_type {
            hardy_bpv7::block::Type::PreviousNode => {
                let (v, shortest) =
                    parse_exact::<(hardy_bpv7::eid::Eid, bool)>(body, "Previous Node Block")?;
                if !shortest {
                    to_update.insert(n, hardy_cbor::encode::emit(&v).0);
                }
            }
            hardy_bpv7::block::Type::BundleAge => {
                parse_exact::<hardy_bpv7::bundle_age::BundleAge>(body, "Bundle Age Block")?;
            }
            hardy_bpv7::block::Type::HopCount => {
                let (v, shortest) =
                    parse_exact::<(hardy_bpv7::hop_info::HopInfo, bool)>(body, "Hop Count Block")?;
                if !shortest {
                    to_update.insert(n, hardy_cbor::encode::emit(&v).0);
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
    hardy_bpv7::rewrite::apply_rewrites(&data, &bundle, keys, to_update, to_remove)
        .map(|opt| opt.map(|(_b, chunks)| chunks))
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
