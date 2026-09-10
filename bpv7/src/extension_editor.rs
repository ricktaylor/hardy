//! Scoped extension-block editing: insert/replace/remove of *extension*
//! blocks only, with every owner privilege absent by construction.
//!
//! [`ExtensionEditor`] wraps an [`Editor`] it never exposes. The wrapper's
//! op set cannot name the primary block, the payload, BIB/BCB blocks, or
//! the primary-field setters — those methods simply do not exist here —
//! and the target gates refuse the remaining reserved cases at call time
//! with a typed [`Error`], so a caller's no-match path is an `Err`, never
//! a review convention. Editing a block under existing BPSec coverage is
//! refused outright (the full [`Editor`] instead strips the target from
//! its coverage — an owner decision this handle deliberately cannot make),
//! and unprovable coverage (undecryptable BIBs) refuses conservatively.
//!
//! Blocks inserted through this editor are valid targets for its own
//! `replace`/`remove`: a fresh insert is by definition an uncovered
//! extension block, so every gate above still holds.
//!
//! Edits accumulate in memory; nothing is materialised until
//! [`finish`](ExtensionEditor::finish).

use alloc::{borrow::Cow, boxed::Box, vec::Vec};

use thiserror::Error;

use crate::{
    block::{BibCoverage, Flags, Type},
    bundle::Bundle,
    crc::CrcType,
    editor::{Chunk, Editor},
};

/// Errors from scoped editing — each refused operation names its reason,
/// so a caller can treat a refusal as its no-match path.
#[derive(Debug, Error)]
pub enum Error {
    /// The operation names a block kind outside the extension scope: the
    /// payload and primary blocks are never editable here, and BIB/BCB
    /// are the BPSec seams' monopoly.
    #[error("An ExtensionEditor cannot edit {0:?} blocks")]
    ReservedType(Type),

    /// The operation targets the primary (0) or payload (1) block.
    #[error("An ExtensionEditor cannot edit block {0}")]
    ReservedBlock(u64),

    /// The target is not one of the bundle's blocks.
    #[error("No such block number {0}")]
    NoSuchBlock(u64),

    /// The target block is under existing BPSec coverage — or may be,
    /// when undecryptable BIBs leave coverage unprovable.
    #[error("Block {0} is under BPSec coverage and cannot be edited")]
    Covered(u64),

    /// A structural editing failure reported by the underlying editor
    /// (illegal duplicate of a singleton type, block numbers exhausted, …).
    #[error(transparent)]
    Editor(#[from] crate::editor::Error),
}

pub type Result<T> = core::result::Result<T, Error>;

/// An extension-block editor over a parsed bundle and its wire bytes.
///
/// See the [module docs](self) for the scope contract. Constructed
/// directly from the parse products; edits are in-memory until
/// [`finish`](Self::finish) materialises them.
pub struct ExtensionEditor<'a> {
    // Consuming-builder inner editor: taken and replaced around each
    // operation. `None` only transiently inside an operation.
    editor: Option<Editor<'a>>,
    edited: bool,
}

impl<'a> ExtensionEditor<'a> {
    /// Builds an editor over the bundle and its resident wire bytes.
    pub fn new(original: &'a Bundle, source_data: &'a [u8]) -> Self {
        Self {
            editor: Some(Editor::new(original, source_data)),
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

    // The scoped refusals for an edit target, checked against the editor's
    // current view — so a block inserted by this editor is a valid target,
    // and a block already removed is not.
    fn check_target(&self, block_number: u64) -> Result<()> {
        if block_number <= 1 {
            return Err(Error::ReservedBlock(block_number));
        }
        let editor = self.editor.as_ref().expect("editor taken re-entrantly");
        let Some((block, _)) = editor.block(block_number) else {
            return Err(Error::NoSuchBlock(block_number));
        };
        if matches!(block.block_type, Type::BlockIntegrity | Type::BlockSecurity) {
            return Err(Error::ReservedType(block.block_type));
        }
        // `Maybe` (undecryptable BIBs of unknown coverage) refuses
        // conservatively: coverage cannot be proven absent.
        if !matches!(block.bib, BibCoverage::None) || block.bcb.is_some() {
            return Err(Error::Covered(block_number));
        }
        Ok(())
    }

    /// Whether any operation has been applied since construction.
    pub fn is_modified(&self) -> bool {
        self.edited
    }

    /// Materialises the accumulated edits: `None` when nothing was edited,
    /// otherwise the rebuilt structural bundle and the chunks that assemble
    /// the rewritten wire form.
    pub fn finish(self) -> Result<Option<(Bundle, Vec<Chunk>)>> {
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
