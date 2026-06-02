/*!
BPSec-aware editing primitives.

Layer 2 of bpv7's editing stack — adds key-aware operations to the
BPSec-agnostic [`crate::editor::Editor`]. The [`BPSecEditor`] extension
trait provides:

- [`BPSecEditor::remove_blocks`] — bulk
  cascade-through-encrypted-BIB block removal. Lenient: silently
  retains the covered targets of any BIB that would be partially shrunk
  without an available Encrypt key, and returns the actually-removed
  set. For naive single-block removal that errors on any BPSec
  involvement, use [`Editor::remove_block`] instead.
- [`BPSecEditor::remove_integrity`] — strip a target block's BIB
  signature, restoring CRC if necessary. Keyless.

and the free function [`remove_encryption`] decrypts a target block and
rewrites it in plaintext, including the §3.8 share-target handshake with
any BIB that covers the decrypted block.

The bare [`crate::editor::Editor`] stays key-free and refuses cascades
through encrypted BIBs with [`crate::editor::Error::BibIsEncrypted`].
The capability is opt-in: callers that want it pull
`use hardy_bpv7::bpsec::edit::BPSecEditor;` into scope; callers that
don't, don't see the methods.

Both the BPA ingress pipeline (via `rewrite::apply_rewrites`) and external
tools / third-party callers go through this trait for anything that
needs a [`key::KeySource`].
*/

use super::*;
use crate::editor::{Editor, EditorBlockSet, Error as EditorError};
use smallvec::SmallVec;

// ===========================================================================
// Extension trait — the public API
// ===========================================================================

/// BPSec-aware operations on an [`Editor`]. See module docs for the
/// capability list. Implemented on [`Editor<'_>`] in this module.
pub trait BPSecEditor: Sized {
    /// Bulk-remove blocks with full BPSec cascade handling. Returns the
    /// editor and the set of block numbers actually removed.
    ///
    /// For naive single-block removal that errors on any BPSec
    /// involvement, use [`Editor::remove_block`] (the inherent method).
    /// For cascade-aware single-block removal, call this with a
    /// 1-element set and inspect the returned set.
    ///
    /// Lenient: when the cascade would partially shrink a BCB-encrypted
    /// BIB and no Encrypt-capable key is available on the protecting
    /// BCB's source EID, the affected BIB's covered targets are silently
    /// retained in the bundle. The alternative — a bundle with BIB
    /// references to removed blocks — would be worse than honouring
    /// fewer `delete_block_on_failure` flags.
    ///
    /// All-dead shrinks (every target of a covering BIB is in `blocks`)
    /// need no Encrypt key: the cascade empties the OperationSet and
    /// recursively drops the BIB.
    #[allow(clippy::result_large_err)]
    fn remove_blocks<K>(
        self,
        blocks: HashSet<u64>,
        key_source: &K,
    ) -> Result<(Self, HashSet<u64>), (Self, EditorError)>
    where
        K: key::KeySource + ?Sized;

    /// Strip the integrity check (BIB) covering a target block. Restores
    /// a CRC on the target if it had none and is no longer BCB-protected.
    ///
    /// Errors with [`Error::NotSigned`] if the target has no covering
    /// BIB. Errors if the covering BIB is itself BCB-encrypted (the BIB
    /// OpSet parse on ciphertext fails) — use [`remove_blocks`] for
    /// that case.
    ///
    /// [`remove_blocks`]: BPSecEditor::remove_blocks
    #[allow(clippy::result_large_err)]
    fn remove_integrity(self, block_number: u64) -> Result<Self, (Self, EditorError)>;
}

impl<'a> BPSecEditor for Editor<'a> {
    fn remove_blocks<K>(
        mut self,
        mut to_remove: HashSet<u64>,
        key_source: &K,
    ) -> Result<(Self, HashSet<u64>), (Self, EditorError)>
    where
        K: key::KeySource + ?Sized,
    {
        // 1. Enumerate the BCB-encrypted BIBs the cascade might touch.
        let encrypted_bibs: Vec<(u64, u64)> = self
            .block_numbers()
            .filter_map(|n| {
                let (block, _) = self.block(n)?;
                if matches!(block.block_type, block::Type::BlockIntegrity) {
                    block.bcb.map(|bcb_n| (n, bcb_n))
                } else {
                    None
                }
            })
            .collect();

        // 2. For each, decrypt the body, decide stage / reencrypt / pull-back.
        let mut staging = CascadeStaging::default();
        let mut decrypted_plaintexts: HashMap<u64, zeroize::Zeroizing<Box<[u8]>>> = HashMap::new();

        for (bib_num, bcb_num) in encrypted_bibs {
            let bcb_opset = match decode_bcb_opset(&self, bcb_num) {
                Ok(Some(opset)) => opset,
                Ok(None) => continue,
                Err(e) => return Err((self, e)),
            };

            // Decrypt + parse the BIB body.
            let (editor, outcome) =
                decrypt_covered_bib(self, &bcb_opset, bib_num, bcb_num, key_source);
            self = editor;
            let (plaintext, bib_opset) = match outcome {
                CoveredBib::NotCovered => continue,
                // NoKey: leave the BIB encrypted. Caller's `to_remove`
                // entries that this BIB protects will hit
                // remove_from_bib_targets failing to parse the ciphertext
                // OpSet and surface as an error.
                CoveredBib::DecryptFailed(Error::NoKey) => continue,
                CoveredBib::DecryptFailed(e) => return Err((self, e.into())),
                CoveredBib::ParseFailed(e) => return Err((self, e)),
                CoveredBib::Decrypted(plaintext, opset) => (plaintext, opset),
            };

            let dead_count = bib_opset
                .operations
                .keys()
                .filter(|t| to_remove.contains(*t))
                .count();
            if dead_count == 0 {
                continue;
            }

            if dead_count < bib_opset.operations.len() {
                // Partial shrink — needs Encrypt-capable key.
                if key_source
                    .key(&bcb_opset.source, &[key::Operation::Encrypt])
                    .is_none()
                {
                    // Pull the BIB's targets back out of to_remove.
                    // Keeping the BIB intact wins over honouring delete
                    // flags on its surviving targets — the alternative
                    // is dangling BIB references.
                    for t in bib_opset.operations.keys() {
                        to_remove.remove(t);
                    }
                    continue;
                }
                staging.reencrypt_bib_nums.push(bib_num);
            }
            // All-dead shrinks fall through here too — the cascade will
            // empty the OpSet and recursively drop the BIB; no
            // re-encryption needed.
            staging.stage_bib_plaintext.push(bib_num);
            decrypted_plaintexts.insert(bib_num, plaintext);
        }

        // 3. Stage plaintext into Editor templates so
        //    remove_from_bib_targets reads plaintext instead of tripping
        //    on ciphertext.
        for &bib_num in &staging.stage_bib_plaintext {
            let plaintext = decrypted_plaintexts
                .get(&bib_num)
                .expect("staged BIB must be in decrypted_plaintexts");
            self = self
                .update_block_inner(bib_num)?
                .with_data(plaintext.as_ref().to_vec().into())
                .rebuild();
        }

        // 4. Cascade. HashSet iteration order is non-deterministic but
        //    the cascade is order-independent: BIB/BCB shrinkage commutes.
        let removed: HashSet<u64> = to_remove.iter().copied().collect();
        for block_number in to_remove {
            self = self.remove_block_inner(block_number)?;
        }

        // 5. Re-encrypt the partial-shrink BIBs that survived.
        for bib_num in staging.reencrypt_bib_nums {
            // The BIB may have been all-dead-shrunk away if its plaintext
            // OpSet was emptied — check before re-encrypting.
            let Some(bcb_num) = self.block(bib_num).and_then(|(b, _)| b.bcb) else {
                continue;
            };
            let bcb_opset = match decode_bcb_opset(&self, bcb_num) {
                Ok(Some(opset)) => opset,
                Ok(None) => continue,
                Err(e) => return Err((self, e)),
            };
            self = match reencrypt_covered_bib(self, bib_num, bcb_num, bcb_opset, key_source) {
                Ok(e) => e,
                Err((ed, e)) => return Err((ed, e.into())),
            };
        }

        Ok((self, removed))
    }

    fn remove_integrity(self, block_number: u64) -> Result<Self, (Self, EditorError)> {
        let Some((target_block, _)) = self.block(block_number) else {
            return Err((self, EditorError::NoSuchBlock(block_number)));
        };
        let block::BibCoverage::Some(bib) = target_block.bib else {
            return Err((self, Error::NotSigned.into()));
        };
        remove_integrity_inner(self, block_number, bib)
    }
}

/// Remove the encryption from a block in the bundle: decrypt the target
/// under its BCB, rewrite it in plaintext, and drop the target from the
/// BCB (removing the BCB entirely if it has no remaining targets).
///
/// Per RFC 9172 Section 3.8 ("A BCB MUST NOT target a BIB unless it shares
/// a security target with that BIB"), any BIB *in the same BCB* that also
/// covers the decrypted block must itself be decrypted and stripped. That
/// path is reached only by future multi-target security contexts:
/// BCB-AES-GCM (RFC 9173) keeps its IV in the context parameters and so
/// cannot share a BCB across targets ([`bcb::Operation::can_share`] is
/// false).
///
/// On error, returns the editor along with the error so it can be reused
/// for recovery.
#[allow(clippy::result_large_err)]
pub fn remove_encryption<'a, K>(
    mut editor: Editor<'a>,
    block_number: u64,
    key_source: &K,
) -> Result<Editor<'a>, (Editor<'a>, EditorError)>
where
    K: key::KeySource + ?Sized,
{
    if block_number == 0 {
        return Err((editor, EditorError::PrimaryBlock));
    }

    let Some((target_block, _)) = editor.block(block_number) else {
        return Err((editor, EditorError::NoSuchBlock(block_number)));
    };
    let Some(bcb) = target_block.bcb else {
        return Err((editor, Error::NotEncrypted.into()));
    };
    let original_block = target_block.clone();

    let opset = match decode_bcb_opset(&editor, bcb) {
        Ok(Some(opset)) => opset,
        Ok(None) => return Ok(editor),
        Err(e) => return Err((editor, e)),
    };

    // The BCB might not actually list this block (inconsistent bundle):
    // nothing to decrypt, leave it untouched.
    let Some(op) = opset.operations.get(&block_number) else {
        return Ok(editor);
    };

    // Decrypt the target payload.
    let block_set = EditorBlockSet { editor };
    let mut target_payload = match op.decrypt(
        key_source,
        bcb::OperationArgs {
            bpsec_source: &opset.source,
            target: block_number,
            source: bcb,
            blocks: &block_set,
        },
    ) {
        Ok(t) => t,
        Err(e) => return Err((block_set.editor, e.into())),
    };
    editor = block_set.editor;

    // Steal the plaintext out of the Zeroizing guard — this is an explicit
    // 'remove the encryption', so dropping the guard is intended.
    let target_payload: Box<[u8]> = core::mem::take(&mut target_payload);

    // Rewrite the block in plaintext. Only restore a CRC when no BIB
    // covers it — RFC 9172 §3.8 (and §4.8 for BCBs) forbid CRCs on
    // BIB/BCB targets. `BibCoverage::Maybe` means a BCB-encrypted BIB
    // exists whose targets we can't enumerate yet; assume the worst (it
    // might cover this block) and leave the CRC off.
    let mut block = editor
        .update_block_inner(block_number)?
        .with_data(target_payload.into_vec().into());
    if matches!(original_block.bib, block::BibCoverage::None)
        && matches!(original_block.crc_type, crc::CrcType::None)
    {
        block = block.with_crc_type(crc::CrcType::CRC32_CASTAGNOLI);
    }
    editor = block.rebuild();

    // §3.8 share-target handshake (multi-target contexts only — see the
    // doc comment): strip any BIB in this BCB that also covers the
    // now-decrypted block.
    if opset.can_share() {
        let bib_targets: SmallVec<[u64; 4]> = opset
            .operations
            .keys()
            .copied()
            .filter(|&target| {
                editor
                    .block(target)
                    .is_some_and(|(blk, _)| matches!(blk.block_type, block::Type::BlockIntegrity))
            })
            .collect();

        for bib_block_num in bib_targets {
            let (ed, outcome) = decrypt_covered_bib(editor, &opset, bib_block_num, bcb, key_source);
            editor = ed;
            let (plaintext, bib_opset) = match outcome {
                CoveredBib::NotCovered => continue,
                // Can't decrypt the BIB — leaving it would violate §3.8.
                CoveredBib::DecryptFailed(_) => {
                    return Err((editor, EditorError::CannotDecryptRelatedBib(bib_block_num)));
                }
                CoveredBib::ParseFailed(e) => return Err((editor, e)),
                CoveredBib::Decrypted(plaintext, opset) => (plaintext, opset),
            };

            // Only act on BIBs that actually cover the decrypted block;
            // others stay encrypted.
            if bib_opset.operations.contains_key(&block_number) {
                // Stage the plaintext so remove_integrity_inner reads it,
                // drop the BIB's now-redundant BCB protection, then strip
                // its signature over the decrypted block.
                editor = editor
                    .update_block_inner(bib_block_num)?
                    .with_data(plaintext.as_ref().to_vec().into())
                    .rebuild();
                editor = editor.remove_from_bcb_targets(bib_block_num, bcb)?;
                editor = remove_integrity_inner(editor, block_number, bib_block_num)?;
            }
        }
    }

    // Drop the decrypted block from the BCB (removing the BCB entirely if
    // it now has no targets).
    editor.remove_from_bcb_targets(block_number, bcb)
}

// ===========================================================================
// Private — cascade staging + post-cascade re-encryption
// ===========================================================================

/// Internal record of which BCB-encrypted BIBs the cascade will touch
/// (`stage_bib_plaintext`) and the subset that survive a partial shrink
/// and therefore need re-encryption (`reencrypt_bib_nums`).
#[derive(Default)]
struct CascadeStaging {
    stage_bib_plaintext: SmallVec<[u64; 4]>,
    reencrypt_bib_nums: SmallVec<[u64; 4]>,
}

/// Decode the BCB's plaintext OperationSet from the editor's current
/// view of the BCB block. Returns `Ok(None)` if the block is missing or
/// has no payload (defensive); `Err` only if the CBOR parse fails.
fn decode_bcb_opset(
    editor: &Editor<'_>,
    bcb_num: u64,
) -> Result<Option<bcb::OperationSet>, EditorError> {
    let Some((_, Some(bcb_payload))) = editor.block(bcb_num) else {
        return Ok(None);
    };
    match hardy_cbor::decode::parse::<bcb::OperationSet>(bcb_payload) {
        Ok(opset) => Ok(Some(opset)),
        Err(e) => Err(crate::error::Error::InvalidField {
            field: "BCB Abstract Syntax Block",
            source: e.into(),
        }
        .into()),
    }
}

/// Outcome of decrypting and parsing a BCB-covered BIB's OperationSet.
/// The failure policy is left to the caller: `NoKey` is a soft skip during
/// a bulk delete but a hard error when decrypting a block for delivery.
enum CoveredBib {
    /// The BCB OperationSet has no entry for this BIB — nothing to do.
    NotCovered,
    /// Decryption failed; carries the BPSec error (e.g. [`Error::NoKey`]).
    DecryptFailed(Error),
    /// Decryption succeeded but the plaintext is not a valid BIB OpSet.
    ParseFailed(EditorError),
    /// Decryption and parse succeeded — the plaintext and its OperationSet.
    Decrypted(zeroize::Zeroizing<Box<[u8]>>, bib::OperationSet),
}

/// Decrypt a BCB-covered BIB body and parse its OperationSet, using the
/// per-target `Operation` already decoded into `bcb_opset`. Threads the
/// editor through (the [`EditorBlockSet`] borrows it for the AAD lookup)
/// and returns it alongside the [`CoveredBib`] outcome.
fn decrypt_covered_bib<'a, K>(
    editor: Editor<'a>,
    bcb_opset: &bcb::OperationSet,
    bib_num: u64,
    bcb_num: u64,
    key_source: &K,
) -> (Editor<'a>, CoveredBib)
where
    K: key::KeySource + ?Sized,
{
    let Some(bcb_op) = bcb_opset.operations.get(&bib_num) else {
        return (editor, CoveredBib::NotCovered);
    };
    let block_set = EditorBlockSet { editor };
    let result = bcb_op.decrypt(
        key_source,
        bcb::OperationArgs {
            bpsec_source: &bcb_opset.source,
            target: bib_num,
            source: bcb_num,
            blocks: &block_set,
        },
    );
    let editor = block_set.editor;
    let plaintext = match result {
        Ok(p) => p,
        Err(e) => return (editor, CoveredBib::DecryptFailed(e)),
    };
    match hardy_cbor::decode::parse::<bib::OperationSet>(&plaintext) {
        Ok(opset) => (editor, CoveredBib::Decrypted(plaintext, opset)),
        Err(e) => (
            editor,
            CoveredBib::ParseFailed(
                crate::error::Error::InvalidField {
                    field: "BIB Abstract Syntax Block",
                    source: e.into(),
                }
                .into(),
            ),
        ),
    }
}

/// Re-encrypt a BCB-covered BIB whose plaintext OperationSet has changed
/// during the cascading block-delete in
/// [`BPSecEditor::remove_blocks`].
///
/// When the cascade drops a non-security block, any BIB referencing it
/// has its OperationSet shrunk. A shrunk BIB that (a) survives the empty
/// check and (b) is itself covered by a BCB has stale ciphertext under
/// that BCB and must be re-encrypted before the bundle is re-serialised.
///
/// Bypasses the [`encryptor::Encryptor`] orchestrator and calls
/// [`rfc9173::bcb_aes_gcm::Operation::encrypt`] directly. AAD inputs
/// (primary block bytes, target/source block headers) are unchanged
/// across the cascade — only the BIB's BTSD differs — so reusing the
/// original `scope_flags`, BCB source EID, and key is sound. The
/// primitive generates a fresh 12-byte IV internally.
///
/// **Caller contract:** the BIB block must have already been updated in
/// `editor` to carry the new plaintext OperationSet bytes.
///
/// Returns `(editor, error)` on encryption failure so the caller can
/// preserve the editor for cleanup.
#[allow(clippy::result_large_err)]
fn reencrypt_covered_bib<'a, K>(
    editor: Editor<'a>,
    bib_block_number: u64,
    bcb_block_number: u64,
    bcb_opset: bcb::OperationSet,
    key_source: &K,
) -> Result<Editor<'a>, (Editor<'a>, Error)>
where
    K: key::KeySource + ?Sized,
{
    let bcb::OperationSet {
        source: bpsec_source,
        mut operations,
    } = bcb_opset;

    // Reuse the existing per-target Operation as a template — block
    // numbers (and therefore AAD inputs) are unchanged across the cascade,
    // so the existing context parameters carry over and `encrypt` produces
    // a fresh entry with rotated per-context state (e.g. AES-GCM IV) plus
    // the ciphertext. The caller must have staged the new BIB plaintext
    // into the editor already, so `EditorBlockSet` surfaces it via
    // `args.blocks.block(target)`.
    // Invariant: `remove_blocks` only re-encrypts a BIB that is a target of
    // this BCB, so the OperationSet always has an entry for it.
    let template_op = operations.get(&bib_block_number).unwrap_or_else(|| {
        panic!(
            "BCB OperationSet must contain the target BIB {bib_block_number} being re-encrypted (logic bug)"
        )
    });
    let editor_bs = EditorBlockSet { editor };
    let result = template_op.encrypt(
        key_source,
        bcb::OperationArgs {
            bpsec_source: &bpsec_source,
            target: bib_block_number,
            source: bcb_block_number,
            blocks: &editor_bs,
        },
    );
    let editor = editor_bs.editor;
    let (new_op, ciphertext) = match result {
        Ok(t) => t,
        Err(e) => return Err((editor, e)),
    };

    // Overwrite the BIB block with ciphertext. The plaintext staged by
    // the caller never reaches wire output — the editor's plan for this
    // block now ends in the ciphertext state.
    let editor = editor
        .update_block_inner(bib_block_number)
        .unwrap_or_else(|(_, e)| {
            // Unreachable: BIB {bib_block_number} is an existing block.
            panic!("update_block on existing BIB {bib_block_number} cannot fail (logic bug): {e}")
        })
        .with_data(ciphertext.into_vec().into())
        .rebuild();

    // Replace the BCB's per-target entry with the fresh Operation and
    // re-emit the OperationSet for the BCB block. `HashMap::insert`
    // overwrites on existing key.
    operations.insert(bib_block_number, new_op);
    let new_bcb_opset = bcb::OperationSet {
        source: bpsec_source,
        operations,
    };
    let editor = editor
        .update_block_inner(bcb_block_number)
        .unwrap_or_else(|(_, e)| {
            // Unreachable: BCB {bcb_block_number} is an existing block.
            panic!("update_block on existing BCB {bcb_block_number} cannot fail (logic bug): {e}")
        })
        .with_data(hardy_cbor::encode::emit(&new_bcb_opset).0.into())
        .rebuild();

    Ok(editor)
}

/// Remove integrity from a block when the BIB block number is already known.
/// Removes the target from the BIB and restores the CRC if needed.
#[allow(clippy::result_large_err)]
fn remove_integrity_inner<'a>(
    mut editor: Editor<'a>,
    block_number: u64,
    bib_block_num: u64,
) -> Result<Editor<'a>, (Editor<'a>, EditorError)> {
    let (has_bcb, needs_crc) = editor
        .block(block_number)
        .map(|(b, _)| (b.bcb.is_some(), matches!(b.crc_type, crc::CrcType::None)))
        .unwrap_or((false, false));

    editor = editor.remove_from_bib_targets(block_number, bib_block_num)?;

    if !has_bcb && needs_crc {
        if block_number == 0 {
            editor = editor.with_bundle_crc_type(crc::CrcType::CRC32_CASTAGNOLI)?;
        } else {
            editor = editor
                .update_block_inner(block_number)?
                .with_crc_type(crc::CrcType::CRC32_CASTAGNOLI)
                .rebuild();
        }
    }

    Ok(editor)
}
