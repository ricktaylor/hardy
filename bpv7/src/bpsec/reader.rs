//! A reader that decrypts BCB-covered blocks on demand, with the `bpsec`
//! feature.

use alloc::boxed::Box;
use core::{cell::OnceCell, ops::ControlFlow};

use zeroize::Zeroizing;

use super::{Decrypted, Error, bcb, key};
use crate::{
    HashMap, block,
    editor::Editor,
    reader::{Availability, PlainReader, Reader},
};

/// The memoised outcome of one covered block's decrypt attempt.
enum Decrypt {
    Plain(Zeroizing<Box<[u8]>>),
    NoKey,
    Failed,
}

/// A [`Reader`] that decrypts BCB-covered blocks on demand, memoising each
/// block's outcome for the reader's lifetime.
///
/// Uncovered blocks read as borrowed wire slices, exactly like
/// [`PlainReader`]. A covered block's first request runs the BCB operation
/// (with [`PlainReader`] serving the AAD lookups) and caches the outcome —
/// plaintext, no-usable-key, or decrypt-failure. Through the [`Reader`]
/// impl every later request replays the cached state, so a covered block
/// is decrypted at most once. The give doors replay a cached plaintext or
/// no-key the same way but re-run a cached failure, since the cache
/// records that a decrypt failed, not why, and they return the exact
/// cause. Cached plaintext is zeroized when the reader drops, so its
/// lifetime bounds how long decrypted bytes stay in memory.
///
/// The two read doors serve different possession needs:
///
/// - The [`Reader`] impl **lends**: `Available` payloads borrow from the
///   wire or from the cache, which is what a chain of consumers sharing
///   one reader wants.
/// - [`block_data`](Self::block_data) **gives**: available plaintext comes
///   back owned (never a cache borrow), and failures come back as typed
///   errors carrying the diagnostic cause. Its consuming twin
///   [`into_block_data`](Self::into_block_data) gives by move, for a
///   caller that reads one block and is then done with the reader.
///
/// The `blocks` and `bcb_ops` given to [`new`](Self::new) MUST be products
/// of the same parse of `source_data`: mixing parse products is a logic
/// error. When a block's coverage index names a BCB that is absent from
/// `bcb_ops`, or whose OperationSet has no operation for the block, the
/// [`Reader`] impl panics (the lending door has no error channel), while the
/// give doors return `Err(Altered)`.
///
/// Over an editor ([`over_editor`](Self::over_editor)) the reader reads
/// the bundle as the editor's staged edits leave it: a block's body is the
/// one the editor holds when it is read, a staged block's data or a kept
/// block's wire bytes, and the editor's current headers and the BCB
/// operation sets its BCB blocks carry as staged decide what to decrypt.
///
/// The memoisation uses interior mutability without locking, so the
/// reader is not `Sync`; share it within one thread of work.
pub struct DecryptingReader<'a> {
    // Private, with the cache derived from them at construction: replacing
    // any of them afterwards would pair the memoised outcomes with other
    // blocks, bytes or keys.
    blocks: &'a HashMap<u64, block::Block>,
    bodies: Bodies<'a>,
    bcb_ops: &'a HashMap<u64, bcb::OperationSet>,
    keys: &'a dyn key::KeySource,
    // One cell per BCB-covered block, memoising its decrypt outcome.
    cache: HashMap<u64, OnceCell<Decrypt>>,
}

// Where a reader's plain block bodies come from.
enum Bodies<'a> {
    // The complete, contiguous wire bytes the blocks' extents index.
    Wire(&'a [u8]),
    // An editor: a staged block's data, or a kept block's wire bytes.
    Editor(&'a Editor<'a>),
}

impl<'a> DecryptingReader<'a> {
    /// Builds a reader over a parsed bundle's blocks, its complete
    /// in-memory bytes, its decoded BCB OperationSets, and a key source.
    pub fn new(
        blocks: &'a HashMap<u64, block::Block>,
        source_data: &'a [u8],
        bcb_ops: &'a HashMap<u64, bcb::OperationSet>,
        keys: &'a dyn key::KeySource,
    ) -> Self {
        Self {
            blocks,
            bodies: Bodies::Wire(source_data),
            bcb_ops,
            keys,
            cache: Self::covered(blocks),
        }
    }

    /// Builds a reader over an editor's staged bundle: `blocks` are the
    /// editor's current block headers
    /// ([`Editor::current_bundle`](crate::editor::Editor::current_bundle))
    /// and `bcb_ops` the operation sets its BCB blocks carry as staged,
    /// each decoded from the body the editor reads for it. The two MUST be
    /// taken from the editor as it stands, with no edit between them.
    ///
    /// A block's body is the one the editor holds when it is read: a
    /// staged block's data, or a kept block's wire bytes. A block the
    /// editor no longer holds reads as not resident.
    pub fn over_editor(
        blocks: &'a HashMap<u64, block::Block>,
        editor: &'a Editor<'a>,
        bcb_ops: &'a HashMap<u64, bcb::OperationSet>,
        keys: &'a dyn key::KeySource,
    ) -> Self {
        Self {
            blocks,
            bodies: Bodies::Editor(editor),
            bcb_ops,
            keys,
            cache: Self::covered(blocks),
        }
    }

    // One empty memo cell per BCB-covered block.
    fn covered(blocks: &HashMap<u64, block::Block>) -> HashMap<u64, OnceCell<Decrypt>> {
        blocks
            .iter()
            .filter(|(_, block)| block.bcb.is_some())
            .map(|(number, _)| (*number, OnceCell::new()))
            .collect()
    }

    /// Seeds the memo with an earlier pass's outcomes over the same parse, so
    /// this reader decrypts none of those blocks again: a seeded plaintext is
    /// lent and given as if this reader had decrypted it, and a seeded
    /// failure reads as [`Availability::NotDecryptable`] through the
    /// [`Reader`] impl (the give doors re-run a failure for its exact cause,
    /// as for any cached one).
    ///
    /// # Panics
    ///
    /// Panics if `decrypted` names a block this parse does not BCB-cover:
    /// the outcomes and the reader's blocks are not products of the same
    /// parse (see the type doc).
    #[must_use]
    pub fn with_decrypted(mut self, decrypted: Decrypted) -> Self {
        let Decrypted { plaintext, failed } = decrypted;
        for (block_number, plaintext) in plaintext {
            self.seed(block_number, Decrypt::Plain(plaintext));
        }
        for block_number in failed {
            self.seed(block_number, Decrypt::Failed);
        }
        self
    }

    /// The reader's decrypt outcomes, seeded or its own, for the next pass
    /// over the same parse. A no-key outcome is not carried: replaying it
    /// costs a key lookup, not a decrypt.
    pub fn into_decrypted(self) -> Decrypted {
        let mut decrypted = Decrypted::default();
        for (block_number, cell) in self.cache {
            match cell.into_inner() {
                Some(Decrypt::Plain(plaintext)) => {
                    decrypted.plaintext.insert(block_number, plaintext);
                }
                Some(Decrypt::Failed) => {
                    decrypted.failed.insert(block_number);
                }
                Some(Decrypt::NoKey) | None => {}
            }
        }
        decrypted
    }

    // Records an earlier pass's outcome for a covered block.
    fn seed(&mut self, block_number: u64, outcome: Decrypt) {
        let cell = self.cache.get_mut(&block_number).unwrap_or_else(|| {
            panic!(
                "block {block_number} was decrypted by an earlier pass but is not BCB-covered here — not products of the same parse"
            )
        });
        let _ = cell.set(outcome);
    }

    // The block's plain body: its wire bytes, or over an editor the body the
    // editor holds. `None` when the body is not resident (`Block::payload`
    // compares the extent in u64, so a block past usize::MAX on a 32-bit
    // target is equally non-resident).
    fn plain_body(&self, block_number: u64, block: &'a block::Block) -> Option<&'a [u8]> {
        match self.bodies {
            Bodies::Wire(source_data) => block.payload(source_data),
            Bodies::Editor(editor) => editor.block(block_number).and_then(|(_, body)| body),
        }
    }

    // Runs the BCB decrypt operation for `block_number` (covered by BCB
    // `bcb_num`), with the plain view the reader serves (a PlainReader over
    // the wire, or the editor) supplying the ciphertext and the AAD lookups.
    // Missing OperationSet entries surface as `Altered`, matching
    // [`block_data`].
    fn decrypt_target(
        &self,
        block_number: u64,
        bcb_num: u64,
    ) -> Result<Zeroizing<Box<[u8]>>, crate::Error> {
        let opset = self.bcb_ops.get(&bcb_num).ok_or(crate::Error::Altered)?;
        let op = opset
            .operations
            .get(&block_number)
            .ok_or(crate::Error::Altered)?;
        let wire;
        let blocks: &dyn Reader<'_> = match self.bodies {
            Bodies::Wire(source_data) => {
                wire = PlainReader {
                    blocks: self.blocks,
                    source_data,
                };
                &wire
            }
            Bodies::Editor(editor) => editor,
        };
        op.decrypt(
            self.keys,
            bcb::OperationArgs {
                source: &opset.source,
                target: block_number,
                security_block: bcb_num,
                blocks,
            },
        )
        .map_err(crate::Error::InvalidBPSec)
    }

    // The shared front of the give doors: `Break` carries the finished
    // answer for a non-resident or uncovered block, `Continue` the number
    // of the BCB covering the block, leaving the decrypt to the caller.
    fn wire_or_covering_bcb(
        &self,
        block_number: u64,
    ) -> Result<ControlFlow<Option<block::Payload<'a>>, u64>, crate::Error> {
        let target = self
            .blocks
            .get(&block_number)
            .ok_or(crate::Error::MissingBlock(block_number))?;
        let Some(body) = self.plain_body(block_number, target) else {
            return Ok(ControlFlow::Break(None));
        };
        match target.bcb {
            Some(bcb_num) => Ok(ControlFlow::Continue(bcb_num)),
            // Unencrypted — the plain body is the plaintext.
            None => Ok(ControlFlow::Break(Some(block::Payload::Borrowed(body)))),
        }
    }

    /// Block `block_number`'s plaintext, owned: `Payload::Borrowed` only
    /// ever slices `source_data` (the uncovered case), and a covered
    /// block's plaintext comes back as an owned `Payload::Decrypted` —
    /// never a borrow of this reader's cache — so the result can outlive
    /// the reader's other borrows and feed zero-copy `Bytes` construction.
    /// A block whose extents lie beyond the resident bytes (the
    /// headers-only or streaming case) returns `Ok(None)`.
    ///
    /// Shares the memo cells with the [`Reader`] impl: a cached plaintext
    /// is cloned out, a cached no-key replays without touching the key
    /// source, and a cached failure re-runs the decrypt so the returned
    /// error carries the exact cause. A caller reading one block and then
    /// dropping the reader uses [`into_block_data`](Self::into_block_data)
    /// instead, which moves the plaintext out rather than cloning it.
    ///
    /// # Errors
    ///
    /// - [`MissingBlock`](crate::Error::MissingBlock) for a block number
    ///   not in the bundle.
    /// - [`InvalidBPSec`](crate::Error::InvalidBPSec) carrying the BPSec
    ///   cause for a covered block with no usable key, or whose decrypt
    ///   fails.
    /// - [`Altered`](crate::Error::Altered) when the coverage index and
    ///   `bcb_ops` disagree (mismatched parse products).
    pub fn block_data(
        &self,
        block_number: u64,
    ) -> Result<Option<block::Payload<'a>>, crate::Error> {
        let bcb_num = match self.wire_or_covering_bcb(block_number)? {
            ControlFlow::Break(answer) => return Ok(answer),
            ControlFlow::Continue(bcb_num) => bcb_num,
        };

        let cell = self
            .cache
            .get(&block_number)
            .expect("the decrypt cache indexes every covered block");
        match cell.get() {
            Some(Decrypt::Plain(plaintext)) => {
                Ok(Some(block::Payload::Decrypted(plaintext.clone())))
            }
            Some(Decrypt::NoKey) => Err(crate::Error::InvalidBPSec(Error::NoKey)),
            // Uncached, or a cached failure: (re-)run the decrypt — a
            // repeat failure is deterministic, and re-running surfaces
            // the exact cause instead of a cached summary.
            Some(Decrypt::Failed) | None => match self.decrypt_target(block_number, bcb_num) {
                Ok(plaintext) => {
                    let payload = block::Payload::Decrypted(plaintext.clone());
                    let _ = cell.set(Decrypt::Plain(plaintext));
                    Ok(Some(payload))
                }
                Err(e) => {
                    match &e {
                        crate::Error::InvalidBPSec(Error::NoKey) => {
                            let _ = cell.set(Decrypt::NoKey);
                        }
                        // Structural mismatch is a precondition violation,
                        // not a decrypt outcome — never cached.
                        crate::Error::Altered => {}
                        _ => {
                            let _ = cell.set(Decrypt::Failed);
                        }
                    }
                    Err(e)
                }
            },
        }
    }

    /// Block `block_number`'s plaintext, consuming the reader: the one-shot
    /// form of [`block_data`](Self::block_data), with the same contract. A
    /// covered block's plaintext moves out of its memo cell, or straight
    /// out of the decrypt, instead of being cloned, so a caller that reads
    /// one block holds a single plaintext copy.
    pub fn into_block_data(
        mut self,
        block_number: u64,
    ) -> Result<Option<block::Payload<'a>>, crate::Error> {
        let bcb_num = match self.wire_or_covering_bcb(block_number)? {
            ControlFlow::Break(answer) => return Ok(answer),
            ControlFlow::Continue(bcb_num) => bcb_num,
        };

        match self
            .cache
            .remove(&block_number)
            .and_then(OnceCell::into_inner)
        {
            Some(Decrypt::Plain(plaintext)) => Ok(Some(block::Payload::Decrypted(plaintext))),
            Some(Decrypt::NoKey) => Err(crate::Error::InvalidBPSec(Error::NoKey)),
            // Uncached, or a cached failure: run the decrypt, as the
            // borrowing door does, for the plaintext or the exact cause.
            Some(Decrypt::Failed) | None => self
                .decrypt_target(block_number, bcb_num)
                .map(|plaintext| Some(block::Payload::Decrypted(plaintext))),
        }
    }
}

impl<'a> Reader<'a> for DecryptingReader<'a> {
    // Panics when a block's coverage index names a BCB absent from
    // `bcb_ops`, or one with no operation for the block. The parser derives
    // the coverage index from the OperationSets themselves, so either means
    // mismatched parse products (see the type doc) — a logic error, not a
    // runtime state — and this infallible door has no error channel to
    // report it through.
    fn block(&'a self, block_number: u64) -> Option<(&'a block::Block, Availability<'a>)> {
        let block = self.blocks.get(&block_number)?;
        let Some(body) = self.plain_body(block_number, block) else {
            return Some((block, Availability::NotResident));
        };
        let Some(bcb_num) = block.bcb else {
            return Some((
                block,
                Availability::Available(block::Payload::Borrowed(body)),
            ));
        };

        let outcome = self
            .cache
            .get(&block_number)
            .expect("the decrypt cache indexes every covered block")
            .get_or_init(|| match self.decrypt_target(block_number, bcb_num) {
                Ok(plaintext) => Decrypt::Plain(plaintext),
                Err(crate::Error::InvalidBPSec(Error::NoKey)) => Decrypt::NoKey,
                Err(crate::Error::Altered) => panic!(
                    "block {block_number} is marked BCB-covered but bcb_ops has no operation for it — blocks and bcb_ops are not products of the same parse"
                ),
                Err(_) => Decrypt::Failed,
            });
        Some((
            block,
            match outcome {
                Decrypt::Plain(plaintext) => {
                    Availability::Available(block::Payload::Borrowed(plaintext))
                }
                Decrypt::NoKey => Availability::NoKey,
                Decrypt::Failed => Availability::NotDecryptable,
            },
        ))
    }

    fn block_header(&'a self, block_number: u64) -> Option<&'a block::Block> {
        self.blocks.get(&block_number)
    }
}
