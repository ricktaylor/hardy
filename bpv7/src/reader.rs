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

/// Provides access to bundle blocks by number, used during BPSec IPPT construction.
pub trait Reader<'a> {
    /// Returns the block and its payload for the given block number, or `None` if absent.
    fn block(&'a self, block_number: u64) -> Option<(&'a Block, Option<Payload<'a>>)>;

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
    fn block(&'a self, block_number: u64) -> Option<(&'a Block, Option<Payload<'a>>)> {
        let block = self.blocks.get(&block_number)?;
        Some((
            block,
            block.payload(self.source_data).map(Payload::Borrowed),
        ))
    }

    fn block_header(&'a self, block_number: u64) -> Option<&'a Block> {
        self.blocks.get(&block_number)
    }
}
