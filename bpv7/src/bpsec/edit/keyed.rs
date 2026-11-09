//! What the BPSec editor reads through encryption, with the `bpsec`
//! feature: the acceptor's removal of a block's encryption, the reading of
//! the encrypted BIBs a removal cascades through, and the resolution of a
//! `Maybe` coverage, each with the editor's key source and plaintext cache.

use alloc::{borrow::Cow, boxed::Box};
use core::mem::take;

use hardy_cbor::decode::parse_exact;
use smallvec::SmallVec;
use zeroize::Zeroizing;

use super::{decode_bcb_opset, encrypted_bibs};
use crate::{
    HashMap, HashSet, block,
    bpsec::{
        Decrypted, bcb, bib,
        key::{self, KeySource},
    },
    editor::{Editor, Error as EditorError},
    reader::{Availability, Reader},
};

// What an editor without a key source decrypts with: nothing, so that an
// operation reports its own failure, `NoKey` in a supported context.
static NO_KEYS: key::KeySet = key::KeySet::EMPTY;

impl<'a> Editor<'a> {
    /// Set the key source this editor decrypts with: the one source for
    /// every operation that reads through encryption, the
    /// [`BPSecEditor`](crate::bpsec::edit::BPSecEditor) methods and the
    /// resolution of a block's [`Maybe`](block::BibCoverage::Maybe)
    /// coverage. An operation that meets such a block decrypts the bundle's
    /// encrypted BIBs until one lists it, and checks that one; supplying
    /// keys says the caller may read the bundle as its destination would.
    /// Without a key source, or where an encrypted BIB that may list the
    /// block cannot be decrypted, the operation refuses with
    /// [`MaybeHasBib`](EditorError::MaybeHasBib), and a decrypt it needs fails
    /// with [`NoKey`](crate::bpsec::Error::NoKey).
    pub fn with_key_source(mut self, key_source: &'a dyn KeySource) -> Self {
        self.key_source = Some(key_source);
        self
    }

    /// Set the plaintext cache this editor reads before it decrypts: the
    /// blocks an earlier pass over the same bundle decrypted, as a security
    /// acceptor's header pass leaves them, so that no block is decrypted
    /// twice. A block the cache holds is written from it, borrowed into the
    /// edit rather than copied; any other is decrypted with the key source.
    pub fn with_decrypted(mut self, decrypted: &'a Decrypted) -> Self {
        self.decrypted = Some(decrypted);
        self
    }
}

// The acceptor's removal of a block's encryption
// ([`BPSecEditor::remove_encryption`](super::BPSecEditor::remove_encryption)).
#[allow(clippy::result_large_err)]
pub(super) fn remove_encryption<'a>(
    editor: Editor<'a>,
    block_number: u64,
) -> Result<Editor<'a>, (Editor<'a>, EditorError)> {
    let (bcb, opset, block_type) = match encryption_of(&editor, block_number) {
        Ok(found) => found,
        Err(e) => return Err((editor, e)),
    };
    let (mut editor, plaintext) = plaintext_of(editor, block_number, bcb, &opset);
    let plaintext = match plaintext {
        Ok(plaintext) => plaintext,
        Err(e) => return Err((editor, e)),
    };

    // The encrypted BIB the removal is coupled to: the block itself when
    // it is a BIB, else the encrypted BIB over it, if any.
    let mut writes: SmallVec<[PlaintextWrite<'a>; 4]> = SmallVec::new();
    let coupled = if matches!(block_type, block::Type::BlockIntegrity) {
        match parse_bib_opset(plaintext.as_slice()) {
            Ok(bib_opset) => Some(CoupledBib {
                block_number,
                bcb,
                bcb_opset: opset,
                plaintext,
                opset: bib_opset,
            }),
            Err(e) => return Err((editor, e)),
        }
    } else {
        writes.push(PlaintextWrite {
            block_number,
            bcb,
            plaintext,
        });
        let (ed, covering) = covering_bib(editor, block_number);
        editor = ed;
        match covering {
            Ok(covering) => covering,
            Err(e) => return Err((editor, e)),
        }
    };

    if let Some(bib) = &coupled {
        let reader = EditorReader { editor };
        let checked = bib
            .opset
            .check_decrypted(bib.block_number, &bib.bcb_opset, &reader);
        editor = reader.editor;
        if let Err(e) = checked {
            return Err((editor, e.into()));
        }

        // The BIB's other targets a BCB covers.
        for &target in bib.opset.operations.keys() {
            if target == block_number {
                continue;
            }
            let Some((_, Some(target_bcb))) = editor.current_coverage(target) else {
                continue;
            };
            let target_opset = match decode_bcb_opset(&editor, target_bcb) {
                Ok(Some(target_opset)) if target_opset.operations.contains_key(&target) => {
                    target_opset
                }
                Ok(_) => continue,
                Err(e) => return Err((editor, e)),
            };
            let (ed, plaintext) = plaintext_of(editor, target, target_bcb, &target_opset);
            editor = ed;
            match plaintext {
                Ok(plaintext) => writes.push(PlaintextWrite {
                    block_number: target,
                    bcb: target_bcb,
                    plaintext,
                }),
                Err(e) => return Err((editor, e)),
            }
        }
    }

    // Record the coverage the decryption reveals: the BIB's targets
    // stay under it, and a block read as `Maybe` that no BIB lists is
    // uncovered.
    match &coupled {
        Some(bib) => {
            for &target in bib.opset.operations.keys() {
                editor.set_bib_target(target, bib.block_number);
            }
        }
        None => {
            if matches!(
                editor.current_coverage(block_number),
                Some((block::BibCoverage::Maybe, _))
            ) {
                editor.clear_bib_target(block_number);
            }
        }
    }
    writes.extend(coupled.map(|bib| PlaintextWrite {
        block_number: bib.block_number,
        bcb: bib.bcb,
        plaintext: bib.plaintext,
    }));

    // Write each block in plaintext and drop it from its BCB. Its CRC
    // type stays as it is, the CRC computed over the plaintext as the
    // block is emitted: no security operation changes a target's CRC.
    for PlaintextWrite {
        block_number: n,
        bcb,
        plaintext,
    } in writes
    {
        let mut block = editor.update_block_inner(n)?;
        if let Some(data) = plaintext.into_data() {
            block = block.with_data(data);
        }
        editor = block.rebuild();
        editor = editor.remove_from_bcb_targets(n, bcb)?;
    }
    Ok(editor)
}

// The BCB that encrypts `block_number` now, its decoded OperationSet, and
// the block's type: what `remove_encryption` checks first. A BCB that cannot
// be read or does not list the block is an inconsistent bundle.
fn encryption_of(
    editor: &Editor<'_>,
    block_number: u64,
) -> Result<(u64, bcb::OperationSet, block::Type), EditorError> {
    if block_number == 0 {
        return Err(EditorError::PrimaryBlock);
    }

    let (Some((target_block, _)), Some((_, bcb))) = (
        editor.block(block_number),
        editor.current_coverage(block_number),
    ) else {
        return Err(EditorError::NoSuchBlock(block_number));
    };
    let Some(bcb) = bcb else {
        return Err(EditorError::NotEncrypted);
    };
    let block_type = target_block.block_type;

    match decode_bcb_opset(editor, bcb)? {
        Some(opset) if opset.operations.contains_key(&block_number) => Ok((bcb, opset, block_type)),
        _ => Err(crate::error::Error::Altered.into()),
    }
}

// A block's plaintext: borrowed from the editor's cache, decrypted here and
// held zeroizing until the block is written with it, or, for a payload
// whose data streams past the editor, the caller's stream's.
enum Plaintext<'a> {
    Cached(&'a [u8]),
    Decrypted(Zeroizing<Box<[u8]>>),
    Streamed,
}

impl<'a> Plaintext<'a> {
    // The plaintext of a BIB, which is always resident: only the payload
    // streams, and it is never a BIB.
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Cached(plaintext) => plaintext,
            Self::Decrypted(plaintext) => plaintext,
            Self::Streamed => unreachable!("a streamed payload is never a BIB"),
        }
    }

    // The block's new data, or `None` for a streamed payload, whose data
    // the caller supplies as it streams. The plaintext leaves its guard
    // here: removing the encryption puts it in the bundle.
    fn into_data(self) -> Option<Cow<'a, [u8]>> {
        match self {
            Self::Cached(plaintext) => Some(Cow::Borrowed(plaintext)),
            Self::Decrypted(mut plaintext) => Some(Cow::Owned(take(&mut *plaintext).into_vec())),
            Self::Streamed => None,
        }
    }
}

// A block the removal writes in plaintext and the BCB it leaves.
struct PlaintextWrite<'a> {
    block_number: u64,
    bcb: u64,
    plaintext: Plaintext<'a>,
}

// The encrypted BIB a removal is coupled to, decrypted.
struct CoupledBib<'a> {
    block_number: u64,
    bcb: u64,
    bcb_opset: bcb::OperationSet,
    plaintext: Plaintext<'a>,
    opset: bib::OperationSet,
}

// What the cascade learnt of the BCB-encrypted BIBs it might touch: the
// targets whose coverage its reads resolved, the BIBs that stayed opaque,
// and the BIBs it read and staged in plaintext.
pub(super) struct EncryptedBibs<'a> {
    pub(super) resolved_targets: HashSet<u64>,
    pub(super) undecrypted: SmallVec<[u64; 4]>,
    staged: SmallVec<[u64; 4]>,
    plaintexts: HashMap<u64, Plaintext<'a>>,
}

impl<'a> EncryptedBibs<'a> {
    // Stages the plaintext of the BIBs the cascade edits into the editor's
    // templates, so the cascade reads plaintext instead of tripping on
    // ciphertext.
    #[allow(clippy::result_large_err)]
    pub(super) fn stage(
        self,
        mut editor: Editor<'a>,
    ) -> Result<Editor<'a>, (Editor<'a>, EditorError)> {
        let EncryptedBibs {
            staged,
            mut plaintexts,
            ..
        } = self;
        for bib_num in staged {
            let data = plaintexts
                .remove(&bib_num)
                .expect("staged BIB must be in plaintexts")
                .into_data()
                .expect("a BIB is resident, so its plaintext is data");
            editor = editor
                .update_block_inner(bib_num)?
                .with_data(data)
                .rebuild();
        }
        Ok(editor)
    }
}

// Reads each BCB-encrypted BIB the cascade might touch, from the editor's
// plaintext cache or decrypted with its key source, checks what the
// decrypt reveals, and stages in plaintext those a removal in `to_remove`
// edits, or refuses.
pub(super) fn stage_encrypted_bibs<'a>(
    mut editor: Editor<'a>,
    to_remove: &HashSet<u64>,
) -> (Editor<'a>, Result<EncryptedBibs<'a>, EditorError>) {
    let mut learnt = EncryptedBibs {
        resolved_targets: HashSet::new(),
        undecrypted: SmallVec::new(),
        staged: SmallVec::new(),
        plaintexts: HashMap::new(),
    };
    for (bib_num, bcb_num) in encrypted_bibs(&editor) {
        let bcb_opset = match decode_bcb_opset(&editor, bcb_num) {
            Ok(Some(opset)) => opset,
            Ok(None) => continue,
            Err(e) => return (editor, Err(e)),
        };

        // Read + parse the BIB body.
        let (ed, outcome) = read_covered_bib(editor, &bcb_opset, bib_num, bcb_num);
        editor = ed;
        let (plaintext, bib_opset) = match outcome {
            CoveredBib::NotCovered => continue,
            // NoKey or DecryptionFailed: leave the BIB encrypted and
            // skip staging. A corrupt (undecryptable) BIB is handled
            // the same as NoKey — it stays as it is here, and is
            // removed wholesale in step 4 if the caller included it
            // in `to_remove` (RFC 9172 §5.1.1 failure-drop). A
            // requested target it may cover is refused unless the BIB
            // itself is going too.
            CoveredBib::DecryptFailed => {
                learnt.undecrypted.push(bib_num);
                continue;
            }
            CoveredBib::ParseFailed(e) => return (editor, Err(e)),
            CoveredBib::Decrypted(plaintext, opset) => (plaintext, opset),
        };
        // What the decrypt reveals is checked here, as the cascade is
        // what decrypted it.
        let reader = EditorReader { editor };
        let checked = bib_opset.check_decrypted(bib_num, &bcb_opset, &reader);
        editor = reader.editor;
        if let Err(e) = checked {
            return (editor, Err(e.into()));
        }
        learnt
            .resolved_targets
            .extend(bib_opset.operations.keys().copied());

        let dead_count = bib_opset
            .operations
            .keys()
            .filter(|t| to_remove.contains(*t))
            .count();
        if dead_count == 0 {
            continue;
        }

        if dead_count < bib_opset.operations.len() && !to_remove.contains(&bib_num) {
            // A partial shrink would rewrite the BIB's ciphertext, which
            // is its BCB's security source's to write.
            let block = *bib_opset
                .operations
                .keys()
                .find(|t| to_remove.contains(*t))
                .expect("a dead target is requested");
            return (
                editor,
                Err(EditorError::ShrinksEncryptedBib {
                    block,
                    bib: bib_num,
                }),
            );
        }
        // All-dead shrinks fall through here, as does a BIB the request
        // names itself, removed wholesale — the cascade empties the
        // OpSet or drops the BIB, and nothing is encrypted again. The
        // decrypt resolves the dead targets' `Maybe` coverage to this
        // BIB, which is what lets the cascade strip them from it; the
        // survivors keep `Maybe`, as a keyless reader still sees them.
        for &target in bib_opset.operations.keys() {
            if to_remove.contains(&target) {
                editor.set_bib_target(target, bib_num);
            }
        }
        learnt.staged.push(bib_num);
        learnt.plaintexts.insert(bib_num, plaintext);
    }
    (editor, Ok(learnt))
}

// A `Maybe` coverage of `block_number` resolved with the editor's key
// source and plaintext cache: the encrypted BIB that lists the block,
// checked once decrypted, or none. It stays `Maybe` while an encrypted BIB
// that may list the block cannot be read.
pub(crate) fn resolve_maybe(
    editor: Editor<'_>,
    block_number: u64,
) -> (Editor<'_>, Result<block::BibCoverage, EditorError>) {
    let (mut editor, covering) = covering_bib(editor, block_number);
    match covering {
        Ok(Some(bib)) => {
            let reader = EditorReader { editor };
            let checked = bib
                .opset
                .check_decrypted(bib.block_number, &bib.bcb_opset, &reader);
            editor = reader.editor;
            if let Err(e) = checked {
                return (editor, Err(e.into()));
            }
            for &target in bib.opset.operations.keys() {
                editor.set_bib_target(target, bib.block_number);
            }
            (editor, Ok(block::BibCoverage::Some(bib.block_number)))
        }
        Ok(None) => {
            editor.clear_bib_target(block_number);
            (editor, Ok(block::BibCoverage::None))
        }
        Err(_) => (editor, Ok(block::BibCoverage::Maybe)),
    }
}

// The encrypted BIB over `block_number`, read, or `None` when none covers
// it. Coverage that reads `Maybe` is resolved by reading the bundle's
// encrypted BIBs in turn until one lists the block; one that cannot be
// decrypted or read leaves the answer open, and its error is returned if no
// other BIB lists the block.
fn covering_bib<'a>(
    mut editor: Editor<'a>,
    block_number: u64,
) -> (Editor<'a>, Result<Option<CoupledBib<'a>>, EditorError>) {
    let candidates: SmallVec<[u64; 4]> = match editor.current_coverage(block_number) {
        Some((block::BibCoverage::Some(bib), _)) => SmallVec::from_slice(&[bib]),
        Some((block::BibCoverage::Maybe, _)) => editor
            .block_numbers()
            .filter(|&n| {
                editor
                    .block(n)
                    .is_some_and(|(b, _)| matches!(b.block_type, block::Type::BlockIntegrity))
            })
            .collect(),
        _ => return (editor, Ok(None)),
    };

    let mut unread = None;
    for bib in candidates {
        let Some((_, Some(bcb))) = editor.current_coverage(bib) else {
            continue;
        };
        let bcb_opset = match decode_bcb_opset(&editor, bcb) {
            Ok(Some(bcb_opset)) if bcb_opset.operations.contains_key(&bib) => bcb_opset,
            Ok(_) => continue,
            Err(e) => return (editor, Err(e)),
        };
        let (ed, plaintext) = plaintext_of(editor, bib, bcb, &bcb_opset);
        editor = ed;
        let read = plaintext.and_then(|plaintext| {
            parse_bib_opset(plaintext.as_slice()).map(|opset| (plaintext, opset))
        });
        match read {
            Ok((plaintext, opset)) if opset.operations.contains_key(&block_number) => {
                let bib = CoupledBib {
                    block_number: bib,
                    bcb,
                    bcb_opset,
                    plaintext,
                    opset,
                };
                return (editor, Ok(Some(bib)));
            }
            Ok(_) => {}
            Err(e) => {
                unread.get_or_insert(e);
            }
        }
    }
    (editor, unread.map_or(Ok(None), Err))
}

// A BCB target's plaintext: the caller's stream, for a payload the editor
// was told streams past it; the editor's cache's, where it holds the block;
// else a decrypt with the editor's key source, or with no keys where none
// is set, under the operation `bcb_opset` holds for it.
fn plaintext_of<'a>(
    editor: Editor<'a>,
    block_number: u64,
    bcb: u64,
    bcb_opset: &bcb::OperationSet,
) -> (Editor<'a>, Result<Plaintext<'a>, EditorError>) {
    if block_number == 1 && editor.streamed_payload().is_some() {
        return (editor, Ok(Plaintext::Streamed));
    }
    if let Some(plaintext) = editor
        .decrypted
        .and_then(|cache| cache.plaintext.get(&block_number))
    {
        return (editor, Ok(Plaintext::Cached(plaintext)));
    }
    let key_source = editor.key_source.unwrap_or(&NO_KEYS);
    let op = bcb_opset
        .operations
        .get(&block_number)
        .expect("the BCB's OperationSet lists the block it covers");
    let reader = EditorReader { editor };
    let result = op.decrypt(
        key_source,
        bcb::OperationArgs {
            source: &bcb_opset.source,
            target: block_number,
            security_block: bcb,
            blocks: &reader,
        },
    );
    (
        reader.editor,
        result.map(Plaintext::Decrypted).map_err(Into::into),
    )
}

/// Outcome of reading a BCB-covered BIB's OperationSet.
/// The failure policy is left to the caller: `NoKey` is a soft skip during
/// a bulk delete but a hard error when decrypting a block for delivery.
enum CoveredBib<'a> {
    /// The BCB OperationSet has no entry for this BIB — nothing to do.
    NotCovered,
    /// No plaintext: decryption failed (NoKey or DecryptionFailed).
    DecryptFailed,
    /// Decryption succeeded but the plaintext is not a valid BIB OpSet.
    ParseFailed(EditorError),
    /// Decryption and parse succeeded — the plaintext and its OperationSet.
    Decrypted(Plaintext<'a>, bib::OperationSet),
}

/// Read a BCB-covered BIB body, from the editor's cache or decrypted with
/// its key source under the per-target `Operation` already decoded into
/// `bcb_opset`, and parse its OperationSet. Threads the editor through (the
/// [`EditorReader`] borrows it for the AAD lookup) and returns it alongside
/// the [`CoveredBib`] outcome.
fn read_covered_bib<'a>(
    editor: Editor<'a>,
    bcb_opset: &bcb::OperationSet,
    bib_num: u64,
    bcb_num: u64,
) -> (Editor<'a>, CoveredBib<'a>) {
    if !bcb_opset.operations.contains_key(&bib_num) {
        return (editor, CoveredBib::NotCovered);
    }
    let (editor, plaintext) = plaintext_of(editor, bib_num, bcb_num, bcb_opset);
    let plaintext = match plaintext {
        Ok(plaintext) => plaintext,
        Err(_) => return (editor, CoveredBib::DecryptFailed),
    };
    match parse_bib_opset(plaintext.as_slice()) {
        Ok(opset) => (editor, CoveredBib::Decrypted(plaintext, opset)),
        Err(e) => (editor, CoveredBib::ParseFailed(e)),
    }
}

/// Parse a decrypted BIB's OperationSet.
fn parse_bib_opset(plaintext: &[u8]) -> Result<bib::OperationSet, EditorError> {
    parse_exact::<bib::OperationSet>(plaintext).map_err(|e| {
        crate::error::Error::InvalidField {
            field: "BIB Abstract Syntax Block",
            source: Box::new(e.into()),
        }
        .into()
    })
}
pub(crate) struct EditorReader<'a> {
    pub(crate) editor: Editor<'a>,
}

impl<'a> Reader<'a> for EditorReader<'a> {
    fn block(&'a self, block_number: u64) -> Option<(&'a block::Block, Availability<'a>)> {
        let (block, payload) = self.editor.block(block_number)?;
        Some((
            block,
            payload
                .map(block::Payload::Borrowed)
                .map_or(Availability::NotResident, Availability::Available),
        ))
    }
}
