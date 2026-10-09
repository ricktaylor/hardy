//! Scoped extension-block editing: insert/replace/remove of *extension*
//! blocks only, with every owner privilege either absent or refused.
//!
//! [`ExtensionEditor`] is a handle on an owner's [`Editor`]: it edits
//! through a borrow it never exposes, and the owner rebuilds the bundle once
//! the handle is gone. Some owner privileges are absent by construction — there are no primary-field
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
//! An edit that would make a malformed bundle is refused at call time
//! instead of failing at the owner's rebuild or at the
//! receiver's decoders. What the structural parser rejects — a
//! `report_on_failure` flag the bundle forbids, an unrecognised CRC type —
//! is refused with the parser's own error. Previous Node, Bundle Age, or
//! Hop Count data that does not decode as its type is refused as
//! [`UndecodableBody`](Error::UndecodableBody): the parser never decodes
//! those bodies, but a receiving BPA does, with the same decoders.
//!
//! Receiver policy over a well-formed bundle is the caller's to decide, not
//! refused here: RFC 9171 §4.4.2's Bundle Age requirement on a bundle
//! without a clock, hop limits, and how a receiver treats blocks it does
//! not support.
//!
//! Edits accumulate in the owner's editor; nothing is materialised until
//! the owner rebuilds. Several handles can edit one owner's editor in turn,
//! each seeing the coverage the edits before it left, and the owner's own
//! edits can come before and after them.

use alloc::{borrow::Cow, boxed::Box};

use hardy_cbor::decode::parse_exact;
use thiserror::Error;

// Aliased `Error`s: this module's own `Error` is the refusal enum below.
use crate::{
    Error as Bpv7Error,
    block::{BibCoverage, Flags, Type},
    bundle_age::BundleAge,
    crc::{CrcType, Error as CrcError},
    editor::{Editor, Error as EditorError},
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

    /// The edit would produce a bundle the structural parser rejects,
    /// refused at call time with the parser's own error: `InvalidFlags` for
    /// a `report_on_failure` flag the bundle forbids (RFC 9171
    /// §4.2.3-4/-5) and `InvalidCrc` for an unrecognised CRC type.
    #[error(transparent)]
    Invalid(#[from] Bpv7Error),

    /// Previous Node, Bundle Age, or Hop Count data that does not decode as
    /// its type. The structural parser never decodes these bodies; a
    /// receiving BPA does, with the same decoders.
    #[error("{block_type:?} block data does not decode")]
    UndecodableBody {
        /// The well-known type the data was checked as.
        block_type: Type,
        /// The type's decode error, boxed to keep every `Result` small.
        #[source]
        source: Box<Bpv7Error>,
    },

    /// A structural editing failure reported by the underlying editor
    /// (illegal duplicate of a singleton type, block numbers exhausted, …).
    #[error(transparent)]
    Editor(#[from] EditorError),
}

pub type Result<T> = core::result::Result<T, Error>;

/// A scoped extension-block editing handle on an owner's [`Editor`].
///
/// See the [module docs](self) for the scope contract. The edits go into
/// the owner's editor, which the owner rebuilds once the handle is gone.
pub struct ExtensionEditor<'e, 'a> {
    editor: &'e mut Editor<'a>,
    edited: bool,
    // The primary block is out of scope, so its verdict is fixed at
    // construction.
    report_on_failure_forbidden: bool,
}

impl<'e, 'a> ExtensionEditor<'e, 'a> {
    /// Builds a handle on `editor`, whose staged edits it starts from.
    pub fn new(editor: &'e mut Editor<'a>) -> Self {
        let report_on_failure_forbidden = editor.current_primary().forbids_report_on_failure();
        Self {
            editor,
            edited: false,
            report_on_failure_forbidden,
        }
    }

    /// Inserts a new extension block, returning its assigned block number.
    ///
    /// Inserting a second instance of a singleton type (Previous Node,
    /// Bundle Age, Hop Count) is refused by the underlying editor; multiple
    /// instances of an [`Unrecognised`](Type::Unrecognised) type are legal.
    /// A `report_on_failure` flag the bundle forbids, an unrecognised CRC
    /// type, and a well-known type's undecodable data are refused.
    ///
    /// # Errors
    ///
    /// - [`ReservedType`](Error::ReservedType) for the primary, payload,
    ///   BIB, and BCB types.
    /// - [`Invalid`](Error::Invalid) for a `report_on_failure` flag the
    ///   bundle forbids or an unrecognised CRC type.
    /// - [`UndecodableBody`](Error::UndecodableBody) for well-known data
    ///   that does not decode as its type.
    /// - [`Editor`](Error::Editor) for a second instance of a singleton type
    ///   or exhausted block numbers.
    pub fn insert(
        &mut self,
        block_type: Type,
        flags: Flags,
        crc_type: CrcType,
        data: Box<[u8]>,
    ) -> Result<u64> {
        // Canonicalized so an `Unrecognised` alias of a reserved code is
        // refused here as the reserved type it encodes, not further down, and
        // an alias of the forbidden flag as the flag it encodes.
        let block_type = block_type.canonicalize();
        let flags = flags.canonicalize();
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

        let block_number = self
            .editor
            .edit_with(|editor| match editor.push_block(block_type) {
                Ok(builder) => {
                    let block_number = builder.block_number();
                    let editor = builder
                        .with_flags(flags)
                        .with_crc_type(crc_type)
                        .with_data(Cow::Owned(data.into_vec()))
                        .rebuild();
                    (editor, Ok(block_number))
                }
                Err((editor, e)) => (editor, Err(e)),
            })?;
        self.edited = true;
        Ok(block_number)
    }

    /// Replaces an extension block's block-specific data, keeping its flags
    /// and CRC type. A well-known type's undecodable data is refused.
    ///
    /// # Errors
    ///
    /// - [`ReservedBlock`](Error::ReservedBlock),
    ///   [`NoSuchBlock`](Error::NoSuchBlock),
    ///   [`ReservedType`](Error::ReservedType), or
    ///   [`Covered`](Error::Covered) for a target outside the scope.
    /// - [`UndecodableBody`](Error::UndecodableBody) for well-known data
    ///   that does not decode as its type.
    pub fn replace(&mut self, block_number: u64, data: Box<[u8]>) -> Result<()> {
        let block_type = self.check_target(block_number)?;
        check_body(block_type, &data)?;

        self.editor
            .edit_with(|editor| match editor.update_block(block_number) {
                Ok(builder) => (
                    builder.with_data(Cow::Owned(data.into_vec())).rebuild(),
                    Ok(()),
                ),
                Err((editor, e)) => (editor, Err(e)),
            })?;
        self.edited = true;
        Ok(())
    }

    /// Removes an extension block.
    ///
    /// # Errors
    ///
    /// [`ReservedBlock`](Error::ReservedBlock),
    /// [`NoSuchBlock`](Error::NoSuchBlock),
    /// [`ReservedType`](Error::ReservedType), or [`Covered`](Error::Covered)
    /// for a target outside the scope.
    pub fn remove(&mut self, block_number: u64) -> Result<()> {
        self.check_target(block_number)?;

        self.editor
            .edit_with(|editor| match editor.remove_block(block_number) {
                Ok(editor) => (editor, Ok(())),
                Err((editor, e)) => (editor, Err(e)),
            })?;
        self.edited = true;
        Ok(())
    }

    // The scoped refusals for an edit target, checked against the editor's
    // current view — so a block inserted by an edit before this one is a
    // valid target, and a block already removed is not — and against its
    // current coverage, which an owner edit before the handle may have
    // changed. Returns the target's type.
    fn check_target(&self, block_number: u64) -> Result<Type> {
        if block_number <= 1 {
            return Err(Error::ReservedBlock(block_number));
        }
        let Some((block, _)) = self.editor.block(block_number) else {
            return Err(Error::NoSuchBlock(block_number));
        };
        if matches!(block.block_type, Type::BlockIntegrity | Type::BlockSecurity) {
            return Err(Error::ReservedType(block.block_type));
        }
        let (bib, bcb) = self
            .editor
            .current_coverage(block_number)
            .ok_or(Error::NoSuchBlock(block_number))?;
        // `Maybe` (undecryptable BIBs of unknown coverage) refuses
        // conservatively: coverage cannot be proven absent.
        if !matches!(bib, BibCoverage::None) || bcb.is_some() {
            return Err(Error::Covered(block_number));
        }
        Ok(block.block_type)
    }

    /// Whether this handle has applied any edit. A refused edit is not one.
    #[must_use]
    pub fn is_modified(&self) -> bool {
        self.edited
    }
}

// A well-known extension block's data must decode as its type, exactly as
// the receive path decodes it: the structural parser never looks inside
// these bodies, so a malformed one would otherwise ship. Other types are
// opaque here.
fn check_body(block_type: Type, data: &[u8]) -> Result<()> {
    let decoded = match block_type {
        Type::PreviousNode => parse_exact::<Eid>(data).map(drop).map_err(Bpv7Error::from),
        Type::BundleAge => parse_exact::<BundleAge>(data).map(drop),
        Type::HopCount => parse_exact::<HopInfo>(data).map(drop),
        _ => Ok(()),
    };
    decoded.map_err(|source| Error::UndecodableBody {
        block_type,
        source: Box::new(source),
    })
}
