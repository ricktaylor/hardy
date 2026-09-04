//! The scoped extension-block editor handed to a [`Rewriter`](super::Rewriter).
//!
//! The handle exposes insert/replace/remove of *extension* blocks only,
//! making payload/primary/BIB/BCB immutability a compile-time property
//! rather than a review promise. Operations are validated and applied as
//! they are called — a refused operation is the rewriter's no-match path —
//! and the engine materialises the accumulated edits into the per-attempt
//! wire form only when the invocation returns [`Verdict::Continue`]
//! (dropping the bundle discards them wholesale).

use alloc::borrow::Cow;

use hardy_bpv7::{
    block::{BibCoverage, Block, Flags, Type},
    bundle,
    crc::CrcType,
    editor::{Chunk, Editor},
};
use thiserror::Error;

use crate::bundle::Bundle;

#[cfg(doc)]
use super::Verdict;

/// Errors from scoped editing — each refused operation names its reason, so
/// a [`Rewriter`](super::Rewriter) can treat a refusal as its no-match path.
#[derive(Debug, Error)]
pub enum Error {
    /// The operation names a block kind outside the Rewriter's scope: the
    /// payload and primary blocks are never filter-editable, and BIB/BCB
    /// are the BPSec seams' monopoly.
    #[error("A Rewriter cannot edit {0:?} blocks")]
    ReservedType(Type),

    /// The operation targets the primary (0) or payload (1) block.
    #[error("A Rewriter cannot edit block {0}")]
    ReservedBlock(u64),

    /// The target is not one of the bundle's blocks.
    #[error("No such block number {0}")]
    NoSuchBlock(u64),

    /// The target block is under existing BPSec coverage — or may be, when
    /// undecryptable BIBs leave coverage unprovable — and this node is not
    /// its security source.
    #[error("Block {0} is under BPSec coverage and cannot be edited")]
    BpsecCovered(u64),

    /// A structural editing failure reported by the underlying editor
    /// (unknown block number, illegal duplicate of a singleton type, …).
    #[error(transparent)]
    Editor(#[from] hardy_bpv7::editor::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

/// The scoped extension-block editor handed to a
/// [`Rewriter`](super::Rewriter).
///
/// Edits are in-memory and per transmission/delivery attempt: the stored
/// bundle is never touched, and each attempt derives its wire form fresh.
pub struct ScopedEditor<'a> {
    bundle: &'a Bundle,
    // Consuming-builder inner editor: taken and replaced around each
    // operation. `None` only transiently inside an operation.
    editor: Option<Editor<'a>>,
    edited: bool,
}

impl<'a> ScopedEditor<'a> {
    /// Builds an editor over the bundle and its resident wire bytes.
    /// Constructed by the engine for each Rewriter invocation.
    pub(crate) fn new(bundle: &'a Bundle, data: &'a [u8]) -> Self {
        Self {
            bundle,
            editor: Some(Editor::new(&bundle.bpv7, data)),
            edited: false,
        }
    }

    /// Inserts a new extension block, returning its assigned block number.
    ///
    /// Inserting a second instance of a singleton type (Previous Node,
    /// Bundle Age, Hop Count) is refused by the underlying editor; multiple
    /// instances of an [`Unrecognised`](Type::Unrecognised) type are legal.
    pub fn insert(
        &mut self,
        block_type: Type,
        flags: Flags,
        crc_type: CrcType,
        data: Box<[u8]>,
    ) -> Result<u64> {
        if matches!(
            block_type,
            Type::Primary | Type::Payload | Type::BlockIntegrity | Type::BlockSecurity
        ) {
            return Err(Error::ReservedType(block_type));
        }

        let editor = self.editor.take().expect("editor taken re-entrantly");
        match editor.push_block(block_type) {
            Ok(builder) => {
                let block_number = builder.block_number();
                self.editor = Some(
                    builder
                        .with_flags(flags)
                        .with_crc_type(crc_type)
                        .with_data(Cow::Owned(data.into_vec()))
                        .rebuild(),
                );
                self.edited = true;
                Ok(block_number)
            }
            Err((editor, e)) => {
                self.editor = Some(editor);
                Err(e.into())
            }
        }
    }

    /// Replaces an extension block's block-specific data, keeping its flags
    /// and CRC type.
    pub fn replace(&mut self, block_number: u64, data: Box<[u8]>) -> Result<()> {
        self.check_target(block_number)?;

        let editor = self.editor.take().expect("editor taken re-entrantly");
        match editor.update_block(block_number) {
            Ok(builder) => {
                self.editor = Some(builder.with_data(Cow::Owned(data.into_vec())).rebuild());
                self.edited = true;
                Ok(())
            }
            Err((editor, e)) => {
                self.editor = Some(editor);
                Err(e.into())
            }
        }
    }

    /// Removes an extension block.
    pub fn remove(&mut self, block_number: u64) -> Result<()> {
        self.check_target(block_number)?;

        let editor = self.editor.take().expect("editor taken re-entrantly");
        match editor.remove_block(block_number) {
            Ok(editor) => {
                self.editor = Some(editor);
                self.edited = true;
                Ok(())
            }
            Err((editor, e)) => {
                self.editor = Some(editor);
                Err(e.into())
            }
        }
    }

    // The scoped refusals for an edit target: only the bundle's own
    // extension blocks are editable — a block inserted by this same
    // invocation is not a target (insert what you meant instead), which
    // keeps every check decidable against the bundle's index.
    fn check_target(&self, block_number: u64) -> Result<()> {
        if block_number <= 1 {
            return Err(Error::ReservedBlock(block_number));
        }
        let Some(block) = self.block(block_number) else {
            return Err(Error::NoSuchBlock(block_number));
        };
        if matches!(block.block_type, Type::BlockIntegrity | Type::BlockSecurity) {
            return Err(Error::ReservedType(block.block_type));
        }
        // `Maybe` (undecryptable BIBs of unknown coverage) refuses
        // conservatively: coverage cannot be proven absent.
        if !matches!(block.bib, BibCoverage::None) || block.bcb.is_some() {
            return Err(Error::BpsecCovered(block_number));
        }
        Ok(())
    }

    fn block(&self, block_number: u64) -> Option<&Block> {
        self.bundle.bpv7.blocks.get(&block_number)
    }

    /// Materialises the accumulated edits: `None` when nothing was edited,
    /// otherwise the rebuilt structural bundle and the chunks that assemble
    /// the rewritten wire form. Consumed by the engine after a
    /// `Verdict::Continue`.
    pub(crate) fn finish(self) -> Result<Option<(bundle::Bundle, Vec<Chunk>)>> {
        if !self.edited {
            return Ok(None);
        }
        self.editor
            .expect("editor taken re-entrantly")
            .rebuild_bundle()
            .map(Some)
            .map_err(Error::Editor)
    }
}
