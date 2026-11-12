/*!
BPSec-aware editing primitives.

Layer 2 of bpv7's editing stack: the [`BPSecEditor`] extension trait adds
to the BPSec-agnostic [`crate::editor::Editor`] the operations that read
through encryption:

- [`BPSecEditor::remove_blocks`](crate::bpsec::edit::BPSecEditor::remove_blocks):
  bulk block removal, cascading through encrypted BIBs, refusing a request
  it cannot honour whole rather than encrypting anything again. For naive
  single-block removal that errors on any BPSec involvement, use
  [`Editor::remove_block`](crate::editor::Editor::remove_block) instead.
- [`BPSecEditor::remove_integrity`](crate::bpsec::edit::BPSecEditor::remove_integrity):
  strip a target block's BIB operation.
- [`BPSecEditor::remove_encryption`](crate::bpsec::edit::BPSecEditor::remove_encryption):
  decrypt a target block and rewrite it in plaintext, together with any
  encrypted BIB over it and that BIB's other encrypted targets, all or
  none.

The keys and the plaintext these read are the editor's: the key source set
by [`Editor::with_key_source`](crate::editor::Editor::with_key_source), and
the cache of plaintext an earlier pass over the bundle decrypted, set by
[`Editor::with_decrypted`](crate::editor::Editor::with_decrypted), so that
no block is decrypted twice. A decrypt neither serves runs with no keys,
and fails as one does: [`NoKey`](crate::bpsec::Error::NoKey) in a context
this node supports. Reading through encryption needs the `bpsec` feature:
without it `remove_encryption` is absent, every encrypted BIB stays opaque
to `remove_blocks`, which refuses a removal the BIB may cover unless the
BIB goes too, and a `Maybe` coverage stays `Maybe`.

The bare [`crate::editor::Editor`] refuses cascades through encrypted BIBs
with [`crate::editor::Error::BibIsEncrypted`]. The capability is opt-in:
callers that want it pull `use hardy_bpv7::bpsec::edit::BPSecEditor;` into
scope; callers that don't, don't see the methods.

Both the BPA's CLA ingress (applying the RFC 9172 §5.1.1 removals its
header pass schedules) and external tools / third-party callers
(including `rewrite::apply_rewrites`) go through this trait for anything
that reads through encryption.
*/

use alloc::boxed::Box;

use hardy_cbor::decode::parse_exact;
use smallvec::SmallVec;

use crate::{
    HashSet, block,
    bpsec::bcb,
    editor::{Editor, Error as EditorError},
};

#[cfg(feature = "bpsec")]
mod keyed;

// Without the `bpsec` feature nothing is decrypted: every BCB-encrypted BIB
// stays opaque, and a `Maybe` coverage stays `Maybe`.
#[cfg(not(feature = "bpsec"))]
mod keyed {
    use smallvec::SmallVec;

    use super::encrypted_bibs;
    use crate::{
        HashSet, block,
        editor::{Editor, Error as EditorError},
    };

    // What the cascade learnt of the BCB-encrypted BIBs it might touch:
    // every one stayed opaque, and no target's coverage was resolved.
    pub(super) struct EncryptedBibs {
        pub(super) resolved_targets: HashSet<u64>,
        pub(super) undecrypted: SmallVec<[u64; 4]>,
    }

    pub(super) fn stage_encrypted_bibs<'a>(
        editor: Editor<'a>,
        _to_remove: &HashSet<u64>,
    ) -> (Editor<'a>, Result<EncryptedBibs, EditorError>) {
        let undecrypted = encrypted_bibs(&editor)
            .into_iter()
            .map(|(bib, _)| bib)
            .collect();
        (
            editor,
            Ok(EncryptedBibs {
                resolved_targets: HashSet::new(),
                undecrypted,
            }),
        )
    }

    pub(crate) fn resolve_maybe(
        editor: Editor<'_>,
        _block_number: u64,
    ) -> (Editor<'_>, Result<block::BibCoverage, EditorError>) {
        (editor, Ok(block::BibCoverage::Maybe))
    }
}

#[cfg(feature = "bpsec")]
pub(crate) use keyed::EditorReader;
pub(crate) use keyed::resolve_maybe;
use keyed::stage_encrypted_bibs;

/// BPSec-aware operations on an [`Editor`]. See module docs for the
/// capability list. Implemented on [`Editor<'_>`] in this module.
pub trait BPSecEditor: Sized {
    /// Bulk-remove blocks with full BPSec cascade handling: every
    /// operation that targets a removed block goes, and a security block
    /// its last operation leaves empty goes with it. Returns the editor and
    /// the set of block numbers removed, which omits numbers the bundle
    /// does not hold.
    ///
    /// For naive single-block removal that errors on any BPSec
    /// involvement, use [`Editor::remove_block`] (the inherent method).
    ///
    /// A BCB-encrypted BIB over a removed block is read, from the editor's
    /// plaintext cache or decrypted with its key source, to find what it
    /// covers, with the `bpsec` feature; without it every encrypted BIB
    /// stays opaque. Where every block it covers goes, it empties and goes too,
    /// and where `blocks` names the BIB itself it goes wholesale, its
    /// operation removed from its BCB without reading its body, as it is
    /// where it cannot be decrypted at all. Nothing is ever encrypted
    /// again.
    ///
    /// Every requested block goes, or nothing does: the request is refused,
    /// the editor returned unchanged but for coverage the decrypts
    /// resolved, where removing a block would
    /// - remove its operation from an encrypted BIB that also covers a
    ///   block that stays, which would have to be encrypted again, as only
    ///   its BCB's security source may
    ///   ([`ShrinksEncryptedBib`](EditorError::ShrinksEncryptedBib));
    /// - leave an encrypted BIB that cannot be decrypted, and may cover the
    ///   block ([`Maybe`](block::BibCoverage::Maybe)), listing a block that
    ///   no longer exists ([`MaybeHasBib`](EditorError::MaybeHasBib));
    /// - name the primary block, the payload block, or a BCB whose targets
    ///   are not all also in `blocks`, which would strand ciphertext that
    ///   reparses as plaintext.
    ///
    /// Every block keeps its CRC type as it stands, the primary block
    /// included: no security operation changes a target's CRC.
    #[allow(clippy::result_large_err)]
    fn remove_blocks(
        self,
        blocks: &HashSet<u64>,
    ) -> Result<(Self, HashSet<u64>), (Self, EditorError)>;

    /// Strip the integrity check (BIB) covering a target block. The target
    /// keeps its CRC type as it stands, the primary block included: no
    /// security operation changes a target's CRC. A non-primary target's
    /// CRC is no part of the operation's input (RFC 9173 §3.7), and the
    /// primary block, whose CRC enters the input of every operation whose
    /// scope includes it through its canonical form (RFC 9172 §4), is
    /// immutable (RFC 9171 §4.3.1). A target whose source removed its CRC,
    /// as RFC 9173 §3.8.1 has a source do, travels on without one.
    ///
    /// Errors with [`EditorError::NotSigned`] if the target has no covering
    /// BIB. A target whose coverage reads
    /// [`Maybe`](block::BibCoverage::Maybe) is first resolved with the
    /// editor's key source and plaintext cache (with the `bpsec` feature),
    /// or refused with
    /// [`MaybeHasBib`](EditorError::MaybeHasBib). Errors if the covering
    /// BIB is itself BCB-encrypted (the BIB OpSet parse on ciphertext
    /// fails); use [`remove_blocks`](Self::remove_blocks) for that case.
    #[allow(clippy::result_large_err)]
    fn remove_integrity(self, block_number: u64) -> Result<Self, (Self, EditorError)>;

    /// Remove the encryption from a block in the bundle, as the acceptor of
    /// its BCB operation (RFC 9172 §5.1.1): decrypt the block, rewrite it
    /// in plaintext, and drop it from its BCB, removing the BCB once it has
    /// no targets.
    ///
    /// The BCB operation on a BIB goes with the BCB operations on that
    /// BIB's targets, all or none, whichever BCBs hold them, so that no
    /// reader meets a plaintext BIB over ciphertext (RFC 9172 §3.9), nor a
    /// plaintext target whose encrypted BIB it cannot see. When an
    /// encrypted BIB covers the block, or the block is itself an encrypted
    /// BIB, that BIB and each of its targets a BCB covers are decrypted
    /// with it. The BIB is checked once decrypted and written in plaintext
    /// with its integrity operations intact: accepting them is a separate
    /// decision, and [`remove_integrity`](Self::remove_integrity) strips
    /// those the caller accepts. A block whose coverage reads
    /// [`Maybe`](block::BibCoverage::Maybe) is resolved by decrypting the
    /// bundle's encrypted BIBs until one lists it.
    ///
    /// A block's plaintext comes from the editor's cache
    /// ([`Editor::with_decrypted`]), as a security acceptor's earlier pass
    /// over the same bundle leaves it, borrowed into the edit rather than
    /// copied, so that no block is decrypted twice; any other block the
    /// removal needs is decrypted with the editor's key source
    /// ([`Editor::with_key_source`]). Every plaintext is gathered before
    /// the first edit, so a failure leaves the editor as it was.
    ///
    /// A payload the editor was told streams past it
    /// ([`Editor::with_streamed_payload`]) is not decrypted here: its
    /// plaintext is the caller's stream, from the payload's
    /// [`bcb::Decryptor`], and the payload block is written for a plaintext
    /// of the declared length, to be framed by
    /// [`Editor::rebuild_streamed`]. The decryptor's
    /// [`finish`](crate::bpsec::bcb::Decryptor::finish) authenticates it,
    /// so the caller must settle it before it keeps the rewritten bundle.
    ///
    /// # Errors
    ///
    /// Every decryption the removal needs must succeed, or nothing is
    /// removed and the editor is returned with the error:
    /// [`NoKey`](crate::bpsec::Error::NoKey) where neither the cache nor
    /// the key source serves a block (an editor without a key source
    /// decrypts with none), or the context's failure, such as
    /// [`DecryptionFailed`](crate::bpsec::Error::DecryptionFailed). That
    /// includes an encrypted BIB read only to learn whether it covers the
    /// block, unless another BIB is found to. A decrypted BIB that fails
    /// its checks returns that error, and a BCB that does not list the
    /// block it covers is [`Altered`](crate::Error::Altered).
    #[cfg(feature = "bpsec")]
    #[allow(clippy::result_large_err)]
    fn remove_encryption(self, block_number: u64) -> Result<Self, (Self, EditorError)>;
}

impl<'a> BPSecEditor for Editor<'a> {
    fn remove_blocks(
        mut self,
        to_remove: &HashSet<u64>,
    ) -> Result<(Self, HashSet<u64>), (Self, EditorError)> {
        // 0. Screen the request. The primary and payload blocks are never
        //    removable, and a BCB may only be removed together with all of
        //    its targets — removing it alone would strand ciphertext that
        //    reparses as plaintext downstream. (BIBs in the request are
        //    fine: removing a BIB wholesale is the RFC 9172 §5.1.1
        //    failure-drop.)
        if to_remove.contains(&0) {
            return Err((self, EditorError::PrimaryBlock));
        }
        if to_remove.contains(&1) {
            return Err((self, EditorError::PayloadBlock));
        }
        for &n in to_remove {
            if !self
                .block(n)
                .is_some_and(|(b, _)| matches!(b.block_type, block::Type::BlockSecurity))
            {
                continue;
            }
            let strands = match decode_bcb_opset(&self, n) {
                Ok(Some(opset)) => opset.operations.keys().any(|t| !to_remove.contains(t)),
                Ok(None) => false,
                Err(e) => return Err((self, e)),
            };
            if strands {
                return Err((self, EditorError::StrandsCiphertext(n)));
            }
        }

        // 1–2. Read the BCB-encrypted BIBs the cascade might touch and stage
        //      what it can read in plaintext, with the `bpsec` feature;
        //      without it every encrypted BIB stays opaque.
        let (editor, learnt) = stage_encrypted_bibs(self, to_remove);
        self = editor;
        let learnt = match learnt {
            Ok(learnt) => learnt,
            Err(e) => return Err((self, e)),
        };

        // 2b. A `Maybe`-covered target must not be removed before its
        //     coverage is resolved: if an encrypted BIB survives
        //     undecrypted, dropping the target could leave that BIB's
        //     ciphertext OperationSet listing a nonexistent block for
        //     downstream key-holders. When every undecryptable BIB is
        //     itself being removed, no stale listing can survive and the
        //     targets may go.
        if learnt.undecrypted.iter().any(|n| !to_remove.contains(n))
            && let Some(&unresolved) = to_remove.iter().find(|&&n| {
                !learnt.resolved_targets.contains(&n)
                    && self
                        .block(n)
                        .is_some_and(|(b, _)| matches!(b.bib, block::BibCoverage::Maybe))
            })
        {
            return Err((self, EditorError::MaybeHasBib(unresolved)));
        }

        // 3. Stage the plaintext the reads recovered into the editor's
        //    templates, so the cascade reads plaintext instead of tripping
        //    on ciphertext.
        #[cfg(feature = "bpsec")]
        {
            self = learnt.stage(self)?;
        }

        // 4. Cascade. HashSet iteration order is non-deterministic but
        //    the cascade is order-independent: BIB/BCB shrinkage commutes.
        //    Report only the requested blocks the bundle actually holds:
        //    phantom numbers must not read as removals (a caller like
        //    `apply_rewrites` treats a non-empty set as "rewritten").
        let removed: HashSet<u64> = to_remove
            .iter()
            .copied()
            .filter(|&n| self.block(n).is_some())
            .collect();
        for &block_number in to_remove {
            self = self.remove_block_inner(block_number)?;
        }

        Ok((self, removed))
    }

    #[cfg(feature = "bpsec")]
    fn remove_encryption(self, block_number: u64) -> Result<Self, (Self, EditorError)> {
        keyed::remove_encryption(self, block_number)
    }

    fn remove_integrity(mut self, block_number: u64) -> Result<Self, (Self, EditorError)> {
        let Some((mut bib, _)) = self.current_coverage(block_number) else {
            return Err((self, EditorError::NoSuchBlock(block_number)));
        };
        if matches!(bib, block::BibCoverage::Maybe) {
            let (editor, resolved) = resolve_maybe(self, block_number);
            self = editor;
            bib = match resolved {
                Ok(bib) => bib,
                Err(e) => return Err((self, e)),
            };
        }
        match bib {
            block::BibCoverage::Some(bib) => self.remove_from_bib_targets(block_number, bib),
            block::BibCoverage::Maybe => Err((self, EditorError::MaybeHasBib(block_number))),
            block::BibCoverage::None => Err((self, EditorError::NotSigned)),
        }
    }
}

// The BCB-encrypted BIBs of the bundle, each with the BCB over it.
fn encrypted_bibs(editor: &Editor<'_>) -> SmallVec<[(u64, u64); 4]> {
    editor
        .block_numbers()
        .filter_map(|n| {
            let (block, _) = editor.block(n)?;
            if matches!(block.block_type, block::Type::BlockIntegrity) {
                block.bcb.map(|bcb_n| (n, bcb_n))
            } else {
                None
            }
        })
        .collect()
}

// ===========================================================================
// Private — cascade staging
// ===========================================================================

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
    match parse_exact::<bcb::OperationSet>(bcb_payload) {
        Ok(opset) => Ok(Some(opset)),
        Err(e) => Err(crate::error::Error::InvalidField {
            field: "BCB Abstract Syntax Block",
            source: Box::new(e.into()),
        }
        .into()),
    }
}
