use super::*;

/// Block Confidentiality Block (BCB) types and operations (RFC 9172 Section 3.7).
pub mod bcb;
/// Block Integrity Block (BIB) types and operations (RFC 9172 Section 3.6).
pub mod bib;
/// Cryptographic key types and key source abstraction for BPSec operations.
pub mod key;

/// BPSec-aware editing primitives ([`BPSecEditor`] extension trait on
/// [`crate::editor::Editor`]). Cascade-through-encrypted-BIB block
/// removal, integrity stripping, and decryption.
///
/// [`BPSecEditor`]: edit::BPSecEditor
pub mod edit;

mod error;
pub use error::Error;

mod parse;

/// RFC 9173 default security contexts (BIB-HMAC-SHA2 and BCB-AES-GCM).
#[cfg(feature = "rfc9173")]
pub mod rfc9173;

// Signer and encryptor always compile. Without any security context
// feature enabled (e.g. rfc9173), their `Context` enums only carry the
// `__Reserved` placeholder variant — callers cannot construct a useful
// context, and the build paths return `Error::UnsupportedOperation`.
/// Bundle encryption API for adding BCB blocks to bundles.
#[cfg(feature = "bpsec")]
pub mod encryptor;
/// Bundle signing API for adding BIB blocks to bundles.
#[cfg(feature = "bpsec")]
pub mod signer;

use crate::error::CaptureFieldErr;

/// A key provider function that returns no keys.
/// Use this when parsing bundles that don't require decryption.
pub fn no_keys(_bundle: &bundle::Bundle, _data: &[u8]) -> Box<dyn key::KeySource> {
    Box::new(key::KeySet::EMPTY)
}

/// BPSec security context identifier (RFC 9172 Section 3.4).
#[derive(Debug, Clone, Copy)]
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
pub enum Context {
    /// BIB-HMAC-SHA2 integrity context (RFC 9173 Section 3).
    #[cfg(feature = "rfc9173")]
    BIB_HMAC_SHA2,
    /// BCB-AES-GCM confidentiality context (RFC 9173 Section 4).
    #[cfg(feature = "rfc9173")]
    BCB_AES_GCM,
    /// A security context ID not recognized by this implementation.
    Unrecognised(u64),
}

impl hardy_cbor::encode::ToCbor for Context {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(match self {
            #[cfg(feature = "rfc9173")]
            Self::BIB_HMAC_SHA2 => &1,
            #[cfg(feature = "rfc9173")]
            Self::BCB_AES_GCM => &2,
            Self::Unrecognised(v) => v,
        })
    }
}

impl hardy_cbor::decode::FromCbor for Context {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, len) = crate::error::parse_canonical::<u64, _>(data, Error::NotCanonical)?;
        Ok((
            match value {
                #[cfg(feature = "rfc9173")]
                1 => Self::BIB_HMAC_SHA2,
                #[cfg(feature = "rfc9173")]
                2 => Self::BCB_AES_GCM,
                value => Self::Unrecognised(value),
            },
            true,
            len,
        ))
    }
}

/// Provides access to bundle blocks by number, used during BPSec IPPT construction.
pub trait BlockSet<'a> {
    /// Returns the block and its payload for the given block number, or `None` if absent.
    fn block(&'a self, block_number: u64)
    -> Option<(&'a block::Block, Option<block::Payload<'a>>)>;

    /// Returns just the block header for the given block number, or `None`
    /// if absent — for callers (e.g. per-OperationSet structural
    /// validation) that need only the header fields, not the payload. The
    /// default delegates to [`block`](BlockSet::block); impls override it
    /// when they can resolve the header without computing the payload.
    fn block_header(&'a self, block_number: u64) -> Option<&'a block::Block> {
        self.block(block_number).map(|(block, _)| block)
    }
}

/// The canonical [`BlockSet`] over a parsed bundle held wholly in memory:
/// a blocks map plus the contiguous bundle bytes the offsets index into.
/// Each block's payload is the raw wire body ([`block::Block::payload`]) —
/// no decryption, no staged rewrites. This is the BlockSet to use when
/// feeding [`block_data`] / signer / encryptor for an in-memory bundle.
pub struct PlainBlockSet<'a> {
    /// The bundle's blocks, keyed by block number (e.g. `Bundle::blocks`).
    pub blocks: &'a HashMap<u64, block::Block>,
    /// The complete, contiguous bundle byte stream the offsets index into.
    pub source_data: &'a [u8],
}

impl<'a> BlockSet<'a> for PlainBlockSet<'a> {
    fn block(
        &'a self,
        block_number: u64,
    ) -> Option<(&'a block::Block, Option<block::Payload<'a>>)> {
        let block = self.blocks.get(&block_number)?;
        Some((
            block,
            block
                .payload(self.source_data)
                .map(block::Payload::Borrowed),
        ))
    }

    fn block_header(&'a self, block_number: u64) -> Option<&'a block::Block> {
        self.blocks.get(&block_number)
    }
}

/// Return block `block_number`'s plaintext: a borrowed slice of
/// `source_data` when the block is unencrypted, or the BCB-decrypted
/// bytes (via `bcb_ops` + `keys`) when it is. `source_data` MUST be the
/// complete in-memory bundle the blocks were parsed from.
///
/// Composes [`PlainBlockSet`] with the BCB decrypt op so consumers
/// (BPA delivery, `bundle` CLI) don't each re-implement it.
pub fn block_data<'a, K>(
    block_number: u64,
    blocks: &'a HashMap<u64, block::Block>,
    source_data: &'a [u8],
    bcb_ops: &HashMap<u64, bcb::OperationSet>,
    keys: &K,
) -> Result<block::Payload<'a>, crate::Error>
where
    K: key::KeySource + ?Sized,
{
    let target = blocks
        .get(&block_number)
        .ok_or(crate::Error::MissingBlock(block_number))?;

    let Some(bcb_num) = target.bcb else {
        // Unencrypted — the raw wire body is the plaintext.
        return target
            .payload(source_data)
            .map(block::Payload::Borrowed)
            .ok_or(crate::Error::Altered);
    };

    let opset = bcb_ops.get(&bcb_num).ok_or(crate::Error::Altered)?;
    let op = opset
        .operations
        .get(&block_number)
        .ok_or(crate::Error::Altered)?;
    op.decrypt(
        keys,
        bcb::OperationArgs {
            bpsec_source: &opset.source,
            target: block_number,
            source: bcb_num,
            blocks: &PlainBlockSet {
                blocks,
                source_data,
            },
        },
    )
    .map(block::Payload::Decrypted)
    .map_err(crate::Error::InvalidBPSec)
}
