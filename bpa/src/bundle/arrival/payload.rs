//! The payload's stages, begun on the resident headers for the drain to
//! feed as the payload streams: its decryption, where a BCB covers it, and
//! the verification of the BIB operations over it.

use hardy_bpv7::{block, bpsec, parse, reader::PlainReader};

use crate::HashMap;

// The payload's verify stage, begun on the resident headers for the drain to
// feed: one streaming verifier per BIB operation over the payload among
// `bibs`, the op-sets whose payload operation `Parsed::check` deferred for
// a payload that was not resident, each paired with its BIB's block number
// for failure attribution.
// The verifiers check the payload's data as received, so a BCB-encrypted
// payload begins none: RFC 9172 §3.9 forbids checking a BIB against
// ciphertext. A BIB with an unsupported security operation is the
// classification's, and one this node holds no key for goes unchecked, a
// verifier's policy skip.
pub(super) fn begin_payload_verification<'a>(
    headers: &[u8],
    keys: &dyn bpsec::key::KeySource,
    blocks: &HashMap<u64, block::Block>,
    bibs: impl Iterator<Item = (u64, &'a bpsec::bib::OperationSet)>,
    target_len: u64,
    strict: bool,
) -> Result<Vec<(u64, bpsec::bib::Verifier)>, hardy_bpv7::Error> {
    let reader = PlainReader {
        blocks,
        source_data: headers,
    };
    let mut verifiers = Vec::new();
    for (bib, ops) in bibs {
        let Some(op) = ops.operations().get(&1) else {
            continue;
        };
        let args = bpsec::bib::OperationArgs {
            source: ops.source(),
            target: 1,
            security_block: bib,
            blocks: &reader,
        };
        match op.begin_verify(keys, &args, target_len) {
            Ok(verifier) => verifiers.push((bib, verifier)),
            Err(
                e @ (bpsec::Error::NoKey
                | bpsec::Error::UnrecognisedContext(_)
                | bpsec::Error::UnsupportedOperation),
            ) if strict => return Err(e.into()),
            Err(
                bpsec::Error::NoKey
                | bpsec::Error::UnrecognisedContext(_)
                | bpsec::Error::UnsupportedOperation,
            ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(verifiers)
}

// The payload's decrypt stage, begun on the resident headers for the drain
// to feed, where this node accepts the payload's BCB: `None` for a payload
// no BCB covers. The key is resolved, and the additional authenticated data
// built, here, so the gate has already reported a missing key
// (`Parsed::check`).
pub fn begin_payload_decryption(
    parsed: &parse::Parsed,
    keys: &dyn bpsec::key::KeySource,
) -> Result<Option<bpsec::bcb::Decryptor>, hardy_bpv7::Error> {
    let blocks = &parsed.bundle.blocks;
    let payload = blocks.get(&1).expect("the payload block exists");
    let Some(bcb) = payload.bcb else {
        return Ok(None);
    };
    let ops = parsed.bcbs.get(&bcb).ok_or(hardy_bpv7::Error::Altered)?;
    let op = ops.operations().get(&1).ok_or(hardy_bpv7::Error::Altered)?;
    let args = bpsec::bcb::OperationArgs {
        source: ops.source(),
        target: 1,
        security_block: bcb,
        blocks: &PlainReader {
            blocks,
            source_data: &parsed.data,
        },
    };
    op.begin_decrypt(keys, &args, payload.data.end - payload.data.start)
        .map(Some)
        .map_err(Into::into)
}

// The payload's verify stage: one streaming verifier per BIB operation
// over the payload among the parse's, over `target_len` bytes of what the
// decrypt stage hands on, the plaintext of an encrypted payload or the data
// of one received in plaintext. Where `strict`, for the payload's acceptor,
// every operation begins, the gate having reported a missing key and an
// unsupported operation, which `Parsed::check` records; otherwise, for a
// verifier, an operation this node holds no key for or cannot process is
// passed over.
pub fn begin_payload_verifiers(
    parsed: &parse::Parsed,
    keys: &dyn bpsec::key::KeySource,
    target_len: u64,
    strict: bool,
) -> Result<Vec<(u64, bpsec::bib::Verifier)>, hardy_bpv7::Error> {
    begin_payload_verification(
        &parsed.data,
        keys,
        &parsed.bundle.blocks,
        parsed.bibs.iter().map(|(&bib, ops)| (bib, ops)),
        target_len,
        strict,
    )
}
