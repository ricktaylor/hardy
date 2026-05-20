/*!
BPSec-aware editing primitives.

Layer 2 of bpv7's editing stack — adds key-aware operations to the
BPSec-agnostic [`crate::editor::Editor`] via the [`BPSecEditor`]
extension trait:

- [`BPSecEditor::remove_blocks`] — bulk
  cascade-through-encrypted-BIB block removal. Lenient: silently
  retains the covered targets of any BIB that would be partially shrunk
  without an available Encrypt key, and returns the actually-removed
  set. For naive single-block removal that errors on any BPSec
  involvement, use [`Editor::remove_block`] instead.
- [`BPSecEditor::remove_integrity`] — strip a target block's BIB
  signature, restoring CRC if necessary. Keyless.
- [`BPSecEditor::remove_encryption`] — decrypt a target block and
  rewrite it in plaintext, including the §3.8 share-target handshake
  with any BIB that covers the decrypted block.

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

    /// Remove the encryption from a block in the bundle.
    ///
    /// Note that this will rewrite (or remove) the target and the BCB
    /// block.
    ///
    /// Per RFC 9172 Section 3.8: "A BCB MUST NOT target a BIB unless it
    /// shares a security target with that BIB." Therefore, when
    /// decrypting a block, any encrypted BIB that targets that block
    /// must also be decrypted and the signature removed.
    ///
    /// On error, returns the editor along with the error so it can be
    /// reused for recovery.
    #[allow(clippy::result_large_err)]
    fn remove_encryption<K>(
        self,
        block_number: u64,
        key_source: &K,
    ) -> Result<Self, (Self, EditorError)>
    where
        K: key::KeySource + ?Sized;
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

            // Decrypt the BIB body.
            let plaintext = {
                let bcb_op = match bcb_opset.operations.get(&bib_num) {
                    Some(op) => op,
                    None => continue,
                };
                let block_set = EditorBlockSet { editor: self };
                let result = bcb_op.decrypt(
                    key_source,
                    bcb::OperationArgs {
                        bpsec_source: &bcb_opset.source,
                        target: bib_num,
                        source: bcb_num,
                        blocks: &block_set,
                    },
                );
                self = block_set.editor;
                match result {
                    Ok(p) => p,
                    // NoKey: leave the BIB encrypted. Caller's `to_remove`
                    // entries that this BIB protects will hit
                    // remove_from_bib_targets failing to parse the
                    // ciphertext OpSet and surface as an error.
                    Err(Error::NoKey) => continue,
                    Err(e) => return Err((self, e.into())),
                }
            };

            // Parse the BIB OperationSet from plaintext.
            let bib_opset = match hardy_cbor::decode::parse::<bib::OperationSet>(&plaintext) {
                Ok(opset) => opset,
                Err(e) => {
                    return Err((
                        self,
                        crate::error::Error::InvalidField {
                            field: "BIB Abstract Syntax Block",
                            source: e.into(),
                        }
                        .into(),
                    ));
                }
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

    fn remove_encryption<K>(
        mut self,
        block_number: u64,
        key_source: &K,
    ) -> Result<Self, (Self, EditorError)>
    where
        K: key::KeySource + ?Sized,
    {
        if block_number == 0 {
            return Err((self, EditorError::PrimaryBlock));
        }

        let Some((target_block, _)) = self.block(block_number) else {
            return Err((self, EditorError::NoSuchBlock(block_number)));
        };

        let Some(bcb) = target_block.bcb else {
            return Err((self, Error::NotEncrypted.into()));
        };

        if let Some((_, Some(bcb_payload))) = self.block(bcb) {
            let original_block = target_block.clone();

            let mut opset = match hardy_cbor::decode::parse::<bcb::OperationSet>(bcb_payload) {
                Ok(opset) => opset,
                Err(e) => {
                    return Err((
                        self,
                        crate::error::Error::InvalidField {
                            field: "BCB Abstract Syntax Block",
                            source: e.into(),
                        }
                        .into(),
                    ));
                }
            };

            if let Some(op) = opset.operations.remove(&block_number) {
                // Decrypt the target payload
                let block_set = EditorBlockSet { editor: self };
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
                    Err(e) => {
                        return Err((block_set.editor, e.into()));
                    }
                };

                // Steal the content of the decrypted payload
                // This is safe as this function is an explicit 'remove the encryption', hence
                // removing the Zeroizing<> is valid
                let target_payload: Box<[u8]> = core::mem::take(&mut target_payload);

                // Replace the block payload
                let mut block = block_set
                    .editor
                    .update_block_inner(block_number)?
                    .with_data(target_payload.into_vec().into());
                // Only restore a CRC when we're sure no BIB covers this
                // block — RFC 9172 §3.8 (and §4.8 for BCBs) forbid CRCs on
                // BIB/BCB targets. `BibCoverage::Maybe` means a BCB-encrypted
                // BIB exists whose targets we can't enumerate yet; assume
                // the worst (it might cover this block) and leave CRC off.
                if matches!(original_block.bib, block::BibCoverage::None)
                    && matches!(original_block.crc_type, crc::CrcType::None)
                {
                    block = block.with_crc_type(crc::CrcType::CRC32_CASTAGNOLI);
                }
                self = block.rebuild();

                // RFC 9172 Section 3.8: "A BCB MUST NOT target a BIB unless it shares a
                // security target with that BIB."
                //
                // Now that we've decrypted block_number and removed it from the BCB targets,
                // any encrypted BIB that targets block_number would violate this rule.
                // We must decrypt such BIBs and remove the signature.

                // Handle BIBs within this same BCB's targets.
                // Note: BCB-AES-GCM (RFC 9173) cannot have multiple targets due to IV
                // uniqueness requirements, so this code path is for future security
                // contexts (e.g., COSE-based) that may support multi-target BCBs.
                if opset.can_share() {
                    let bib_targets: SmallVec<[u64; 4]> = opset
                        .operations
                        .keys()
                        .filter(|&&target| {
                            if let Some((blk, _)) = self.block(target) {
                                matches!(blk.block_type, block::Type::BlockIntegrity)
                            } else {
                                false
                            }
                        })
                        .copied()
                        .collect();

                    for bib_block_num in bib_targets {
                        let Some(bib_op) = opset.operations.get(&bib_block_num) else {
                            continue;
                        };

                        // Decrypt the BIB to inspect its targets
                        let block_set = EditorBlockSet { editor: self };
                        let mut decrypted_bib = match bib_op.decrypt(
                            key_source,
                            bcb::OperationArgs {
                                bpsec_source: &opset.source,
                                target: bib_block_num,
                                source: bcb,
                                blocks: &block_set,
                            },
                        ) {
                            Ok(t) => t,
                            Err(_) => {
                                // We can't decrypt the BIB - this would leave the bundle in an
                                // invalid state per RFC 9172 Section 3.8
                                return Err((
                                    block_set.editor,
                                    EditorError::CannotDecryptRelatedBib(bib_block_num),
                                ));
                            }
                        };
                        self = block_set.editor;

                        // Parse the decrypted BIB to check its targets
                        let bib_opset =
                            match hardy_cbor::decode::parse::<bib::OperationSet>(&decrypted_bib) {
                                Ok(opset) => opset,
                                Err(e) => {
                                    return Err((
                                        self,
                                        crate::error::Error::InvalidField {
                                            field: "BIB Abstract Syntax Block",
                                            source: e.into(),
                                        }
                                        .into(),
                                    ));
                                }
                            };

                        // Check if the BIB targets the block we just decrypted
                        if bib_opset.operations.contains_key(&block_number) {
                            // The BIB targets our decrypted block - decrypt the BIB and remove signature
                            let decrypted_bib: Box<[u8]> = core::mem::take(&mut decrypted_bib);
                            self = self
                                .update_block_inner(bib_block_num)?
                                .with_data(decrypted_bib.into_vec().into())
                                .rebuild();

                            // Remove the BIB from the BCB's target list
                            opset.operations.remove(&bib_block_num);

                            // Now remove the signature from the decrypted block (and restore CRC)
                            self = remove_integrity_inner(self, block_number, bib_block_num)?;
                        }
                        // If BIB doesn't target our block, leave it encrypted
                    }
                }

                // Update/remove the current BCB
                if opset.operations.is_empty() {
                    self = self.remove_block_inner(bcb)?;
                } else {
                    // Rewrite BCB
                    self = self
                        .update_block_inner(bcb)?
                        .with_data(hardy_cbor::encode::emit(&opset).0.into())
                        .rebuild();
                }
            }
        }

        Ok(self)
    }
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
    let template_op = operations.get(&bib_block_number).unwrap_or_else(|| {
        panic!(
            "reencrypt_covered_bib: BCB OperationSet missing entry for target BIB {bib_block_number}"
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
            panic!(
                "Editor update_block_inner({bib_block_number}) failed overwriting BIB with ciphertext: {e}"
            )
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
            panic!(
                "Editor update_block_inner({bcb_block_number}) failed writing re-encrypted BCB OperationSet: {e}"
            )
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
