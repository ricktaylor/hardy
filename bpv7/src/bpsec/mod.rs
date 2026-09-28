use alloc::boxed::Box;
use core::{cell::OnceCell, ops::ControlFlow};

use hardy_cbor::{
    decode::FromCbor,
    encode::{Encoder, ToCbor},
};
use zeroize::Zeroizing;
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

// `crate::Error` is written qualified throughout, deliberately: this
// module's own `Error` (re-exported above) takes the bare name.
use crate::{
    HashMap, block, bundle,
    error::CaptureFieldErr,
    reader::{Availability, PlainReader, Reader},
};

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

impl ToCbor for Context {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(match self {
            #[cfg(feature = "rfc9173")]
            Self::BIB_HMAC_SHA2 => &1,
            #[cfg(feature = "rfc9173")]
            Self::BCB_AES_GCM => &2,
            Self::Unrecognised(v) => v,
        })
    }
}

impl FromCbor for Context {
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
/// error. When a block's coverage index names a BCB whose OperationSet has
/// no operation for it, the [`Reader`] impl panics (the lending door has no
/// error channel), while the give doors return `Err(Altered)`.
///
/// The memoisation uses interior mutability without locking, so the
/// reader is not `Sync`; share it within one thread of work.
pub struct DecryptingReader<'a> {
    // Private, with the cache derived from them at construction: replacing
    // any of them afterwards would pair the memoised outcomes with other
    // blocks, bytes or keys.
    blocks: &'a HashMap<u64, block::Block>,
    source_data: &'a [u8],
    bcb_ops: &'a HashMap<u64, bcb::OperationSet>,
    keys: &'a dyn key::KeySource,
    // One cell per BCB-covered block, memoising its decrypt outcome.
    cache: HashMap<u64, OnceCell<Decrypt>>,
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
            source_data,
            bcb_ops,
            keys,
            cache: blocks
                .iter()
                .filter(|(_, block)| block.bcb.is_some())
                .map(|(number, _)| (*number, OnceCell::new()))
                .collect(),
        }
    }

    // The block's payload extents lie within the resident bytes. Compared
    // in u64: a block past usize::MAX on a 32-bit target is equally
    // non-resident.
    fn is_resident(&self, block: &block::Block) -> bool {
        block.payload_range().end <= self.source_data.len() as u64
    }

    // Runs the BCB decrypt operation for `block_number` (covered by BCB
    // `bcb_num`), with PlainReader serving the AAD lookups. Missing
    // OperationSet entries surface as `Altered`, matching [`block_data`].
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
        op.decrypt(
            self.keys,
            bcb::OperationArgs {
                bpsec_source: &opset.source,
                target: block_number,
                source: bcb_num,
                blocks: &PlainReader {
                    blocks: self.blocks,
                    source_data: self.source_data,
                },
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
        if !self.is_resident(target) {
            return Ok(ControlFlow::Break(None));
        }
        match target.bcb {
            Some(bcb_num) => Ok(ControlFlow::Continue(bcb_num)),
            // Unencrypted — the raw wire body is the plaintext.
            None => target
                .payload(self.source_data)
                .map(|payload| ControlFlow::Break(Some(block::Payload::Borrowed(payload))))
                .ok_or(crate::Error::Altered),
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
    // Panics when a block's coverage index names a BCB with no operation
    // for it. The parser derives the coverage index from the OperationSets
    // themselves, so that means mismatched parse products (see the type
    // doc) — a logic error, not a runtime state — and this infallible door
    // has no error channel to report it through.
    fn block(&'a self, block_number: u64) -> Option<(&'a block::Block, Availability<'a>)> {
        let block = self.blocks.get(&block_number)?;
        if !self.is_resident(block) {
            return Some((block, Availability::NotResident));
        }
        let Some(bcb_num) = block.bcb else {
            return Some((
                block,
                block
                    .payload(self.source_data)
                    .map(block::Payload::Borrowed)
                    .map_or(Availability::NotResident, Availability::Available),
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
                    "block {block_number} is marked BCB-covered but its OperationSet has no operation for it — blocks and bcb_ops are not products of the same parse"
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
