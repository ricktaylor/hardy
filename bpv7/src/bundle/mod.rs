/*!
This module defines the core bundle data model: the [`Bundle`] structure
(primary block + blocks map) together with its identifying types
([`BundleId`], [`Flags`], [`FragmentInfo`]). The wire parser lives in
[`crate::parse`]; the BPSec validation/transform primitives in
[`crate::checks`] and [`crate::rewrite`]. Semantic bundle comparison
([`Bundle::semantic_eq`]) lives in the private `compare` submodule.
*/

use alloc::{collections::BTreeMap, vec::Vec};

use crate::HashMap;

mod block;
mod block_flags;
mod bundle_flags;
mod compare;
mod id;
mod primary_block;

pub use self::{
    block::{BibCoverage, Block, BlockType, Payload},
    block_flags::BlockFlags,
    bundle_flags::BundleFlags,
    id::{BundleId, BundleIdError, FragmentInfo},
    primary_block::PrimaryBlock,
};

/// A parsed BPv7 bundle: the primary block plus the extension and payload
/// blocks keyed by block number. This is the crate's structural bundle
/// representation, produced by [`parse`](crate::parse::parse) and emitted
/// by [`Builder`](crate::builder::Builder) / [`Editor`](crate::editor::Editor).
///
/// The derived `==` is structural and offset-sensitive — block extents are
/// buffer-relative, so re-encodings of the same bundle compare unequal. For
/// data-aware, offset-insensitive equivalence use
/// [`semantic_eq`](Self::semantic_eq).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Bundle {
    /// The bundle's primary block, decoded into typed fields.
    pub primary: PrimaryBlock,
    /// All blocks keyed by wire block number (primary = 0, payload = 1,
    /// extensions = 2+). `blocks[0]` is the primary block in raw form, kept for
    /// byte-exact access (e.g. BPSec primary-block AAD); [`primary`](Self::primary)
    /// above is the same block decoded.
    pub blocks: HashMap<u64, Block>,
}

impl Bundle {
    /// The exact byte length of the bundle's encoded form, derived from
    /// the block index alone.
    ///
    /// Two RFC 9171 §4.1 mandates make this a pure function of the payload
    /// block's extent: the payload block MUST be the last block of the
    /// bundle, and a bundle SHALL be an indefinite-length CBOR array,
    /// closed by exactly one "break" byte after the last block. The
    /// encoded length is therefore the payload block's bundle-absolute
    /// [`extent`](Block::extent) end plus one.
    ///
    /// Canonical CBOR gives the payload a definite-length head, so the
    /// value is known once the block headers have been parsed — no payload
    /// bytes, loaded buffer, or stored measurement is needed.
    ///
    /// # Panics
    ///
    /// Panics if the bundle has no payload block (block number 1); every
    /// bundle produced by [`parse`](crate::parse::parse) or
    /// [`Builder`](crate::builder::Builder) has exactly one.
    pub fn encoded_len(&self) -> u64 {
        self.blocks
            .get(&1)
            .expect("bundle has no payload block")
            .extent
            .end
            + 1
    }

    /// Group block numbers by type code, each list sorted ascending. The
    /// primary block (number 0) is handled separately and excluded.
    pub fn blocks_by_type(&self) -> BTreeMap<u64, (BlockType, Vec<u64>)> {
        let mut map: BTreeMap<u64, (BlockType, Vec<u64>)> = BTreeMap::new();
        for (&bn, blk) in &self.blocks {
            if bn == 0 {
                continue;
            }
            let type_code: u64 = blk.block_type.into();
            map.entry(type_code)
                .or_insert_with(|| (blk.block_type, Vec::new()))
                .1
                .push(bn);
        }
        for v in map.values_mut() {
            v.1.sort();
        }
        map
    }
}
