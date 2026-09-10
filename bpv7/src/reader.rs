//! Read access to a bundle's blocks by number.
//!
//! [`Reader`] is the crate's read abstraction over a bundle's blocks: block
//! headers and block payloads, looked up by block number. It is consumed by
//! the BPSec machinery (IPPT/AAD construction walks the target blocks of an
//! operation through a `Reader`) and by any caller that wants uniform block
//! access without caring how the bytes are held.
//!
//! Implementations differ in where the payload bytes come from:
//!
//! - [`PlainReader`] — the raw wire body of a parsed, wholly in-memory
//!   bundle; no decryption, no staged rewrites.
//! - The editor's internal reader — staged (unmaterialised) rewrites.
//!
//! For BCB-covered payloads decrypted on demand, see
//! [`bpsec::block_data`](crate::bpsec::block_data), which composes a
//! [`PlainReader`] with the BCB decrypt operation.

use crate::{
    HashMap,
    block::{Block, Payload},
};

/// The outcome of asking a [`Reader`] for a block's payload.
///
/// A present block's payload can be unavailable for three distinct reasons,
/// and a caller's correct response differs for each — a policy filter may
/// treat an undecryptable block as tampering evidence while passing over one
/// it merely holds no key for — so the states are never conflated into a
/// bare `None`. This is the *outcome* axis only: how available bytes are
/// held (borrowed slice vs owned decrypted buffer) remains [`Payload`]'s
/// concern, wrapped in [`Available`](Self::Available).
#[derive(Debug)]
pub enum Availability<'a> {
    /// The payload bytes are available.
    Available(Payload<'a>),
    /// The block's extents lie outside the resident bytes — the
    /// headers-only or streaming case.
    NotResident,
    /// The block is BCB-covered and no usable key is held.
    NoKey,
    /// The block is BCB-covered and decryption was attempted and failed.
    NotDecryptable,
}

impl<'a> Availability<'a> {
    /// The payload when [`Available`](Self::Available), otherwise `None`.
    ///
    /// For callers to whom every unavailable state means the same thing —
    /// IPPT construction treats them all as a missing security target.
    /// Callers that respond differently per state match the enum instead.
    pub fn available(self) -> Option<Payload<'a>> {
        match self {
            Availability::Available(payload) => Some(payload),
            _ => None,
        }
    }
}

/// Provides access to bundle blocks by number, used during BPSec IPPT construction.
pub trait Reader<'a> {
    /// Returns the block and its payload's [`Availability`] for the given
    /// block number, or `None` if the bundle has no such block.
    fn block(&'a self, block_number: u64) -> Option<(&'a Block, Availability<'a>)>;

    /// Returns just the block header for the given block number, or `None`
    /// if absent — for callers (e.g. per-OperationSet structural
    /// validation) that need only the header fields, not the payload. The
    /// default delegates to [`block`](Reader::block); impls override it
    /// when they can resolve the header without computing the payload.
    fn block_header(&'a self, block_number: u64) -> Option<&'a Block> {
        self.block(block_number).map(|(block, _)| block)
    }
}

/// The canonical [`Reader`] over a parsed bundle held wholly in memory:
/// a blocks map plus the contiguous bundle bytes the offsets index into.
/// Each block's payload is the raw wire body ([`Block::payload`]) —
/// no decryption, no staged rewrites. This is the Reader to use when
/// feeding [`bpsec::block_data`](crate::bpsec::block_data) / signer /
/// encryptor for an in-memory bundle.
pub struct PlainReader<'a> {
    /// The bundle's blocks, keyed by block number (e.g. `Bundle::blocks`).
    pub blocks: &'a HashMap<u64, Block>,
    /// The complete, contiguous bundle byte stream the offsets index into.
    pub source_data: &'a [u8],
}

impl<'a> Reader<'a> for PlainReader<'a> {
    fn block(&'a self, block_number: u64) -> Option<(&'a Block, Availability<'a>)> {
        let block = self.blocks.get(&block_number)?;
        Some((
            block,
            block
                .payload(self.source_data)
                .map(Payload::Borrowed)
                .map_or(Availability::NotResident, Availability::Available),
        ))
    }

    fn block_header(&'a self, block_number: u64) -> Option<&'a Block> {
        self.blocks.get(&block_number)
    }
}
