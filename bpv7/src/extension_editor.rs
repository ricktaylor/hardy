//! Scoped extension-block editing: insert/replace/remove of *extension*
//! blocks only, with every owner privilege either absent or refused.
//!
//! [`ExtensionEditor`] wraps an [`Editor`] it never exposes. Some owner
//! privileges are absent by construction — there are no primary-field
//! setters and no way to add or manage a BIB or BCB — and the target gates
//! refuse the rest at call time with a typed [`Error`](enum@Error): the
//! primary and payload blocks, the reserved block types, and security
//! blocks as targets. A caller's no-match path is an `Err`, never a review
//! convention. Editing a block under existing BPSec coverage is
//! refused outright (the full [`Editor`] instead strips the target from
//! its coverage — an owner decision this handle deliberately cannot make),
//! and unprovable coverage (undecryptable BIBs) refuses conservatively.
//!
//! Blocks inserted through this editor are valid targets for its own
//! `replace`/`remove`: a fresh insert is by definition an uncovered
//! extension block, so every gate above still holds.
//!
//! An edit that would fail at [`finish`](ExtensionEditor::finish) or yield
//! bytes a receiver rejects is refused at call time instead, carrying the
//! error the rejecting check raises. The parser rejects a
//! `report_on_failure` flag the bundle forbids and an unrecognised CRC
//! type. It never decodes Previous Node, Bundle Age, or Hop Count data, but
//! a receiving BPA does, with the same decoders, so data that does not
//! decode as its type is refused with that decode error.
//!
//! Edits accumulate in memory; nothing is materialised until
//! [`finish`](ExtensionEditor::finish).

use alloc::{borrow::Cow, boxed::Box, vec::Vec};

use hardy_cbor::decode::parse_exact;
use thiserror::Error;

// Aliased `Error`s: this module's own `Error` is the refusal enum below.
use crate::{
    Error as Bpv7Error,
    block::{BibCoverage, Flags, Type},
    bundle::Bundle,
    bundle_age::BundleAge,
    crc::{CrcType, Error as CrcError},
    editor::{Chunk, Editor, Error as EditorError},
    eid::Eid,
    hop_info::HopInfo,
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

    /// The edit would produce a bundle a receiver rejects, refused at call
    /// time with the error the rejecting check raises: the parser's
    /// `InvalidFlags` for a `report_on_failure` flag the bundle forbids
    /// (RFC 9171 §4.2.3-4/-5) and `InvalidCrc` for an unrecognised CRC
    /// type, or the type's own decode error for Previous Node, Bundle Age,
    /// or Hop Count data that does not decode as its type.
    #[error(transparent)]
    Invalid(#[from] Bpv7Error),

    /// A structural editing failure reported by the underlying editor
    /// (illegal duplicate of a singleton type, block numbers exhausted, …).
    #[error(transparent)]
    Editor(#[from] EditorError),
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
    // The primary block is out of scope, so its verdict is fixed at
    // construction.
    report_on_failure_forbidden: bool,
}

impl<'a> ExtensionEditor<'a> {
    /// Builds an editor over the bundle and its resident wire bytes.
    pub fn new(original: &'a Bundle, source_data: &'a [u8]) -> Self {
        Self {
            editor: Some(Editor::new(original, source_data)),
            edited: false,
            report_on_failure_forbidden: original.primary.forbids_report_on_failure(),
        }
    }

    /// Inserts a new extension block, returning its assigned block number.
    ///
    /// Inserting a second instance of a singleton type (Previous Node,
    /// Bundle Age, Hop Count) is refused by the underlying editor; multiple
    /// instances of an [`Unrecognised`](Type::Unrecognised) type are legal.
    /// A `report_on_failure` flag the bundle forbids, an unrecognised CRC
    /// type, and a well-known type's undecodable data are refused.
    pub fn insert(
        &mut self,
        block_type: Type,
        flags: Flags,
        crc_type: CrcType,
        data: Box<[u8]>,
    ) -> Result<u64> {
        // Canonicalized so an `Unrecognised` alias of a reserved code is
        // refused here as the reserved type it encodes, not further down.
        let block_type = block_type.canonicalize();
        if matches!(
            block_type,
            Type::Primary | Type::Payload | Type::BlockIntegrity | Type::BlockSecurity
        ) {
            return Err(Error::ReservedType(block_type));
        }
        if flags.report_on_failure && self.report_on_failure_forbidden {
            return Err(Bpv7Error::InvalidFlags.into());
        }
        if let CrcType::Unrecognised(code) = crc_type {
            return Err(Bpv7Error::InvalidCrc(CrcError::InvalidType(code)).into());
        }
        check_body(block_type, &data)?;

        let editor = self
            .editor
            .take()
            .expect("the editor is present between operations");
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
    /// and CRC type. A well-known type's undecodable data is refused.
    pub fn replace(&mut self, block_number: u64, data: Box<[u8]>) -> Result<()> {
        let block_type = self.check_target(block_number)?;
        check_body(block_type, &data)?;

        let editor = self
            .editor
            .take()
            .expect("the editor is present between operations");
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

        let editor = self
            .editor
            .take()
            .expect("the editor is present between operations");
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
    // and a block already removed is not. Returns the target's type.
    fn check_target(&self, block_number: u64) -> Result<Type> {
        if block_number <= 1 {
            return Err(Error::ReservedBlock(block_number));
        }
        let editor = self
            .editor
            .as_ref()
            .expect("the editor is present between operations");
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
        Ok(block.block_type)
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
            .expect("the editor is present between operations")
            .rebuild_bundle()
            .map(Some)
            .map_err(Error::Editor)
    }
}

// A well-known extension block's data must decode as its type, exactly as
// the receive path decodes it: the structural parser never looks inside
// these bodies, so a malformed one would otherwise ship. Other types are
// opaque here.
fn check_body(block_type: Type, data: &[u8]) -> core::result::Result<(), Bpv7Error> {
    match block_type {
        Type::PreviousNode => parse_exact::<Eid>(data).map(drop)?,
        Type::BundleAge => parse_exact::<BundleAge>(data).map(drop)?,
        Type::HopCount => parse_exact::<HopInfo>(data).map(drop)?,
        _ => {}
    }
    Ok(())
}
