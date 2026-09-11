/*!
The oversized-payload continuation ([`PayloadTail`]) and the block-trailer
(CRC) consumption helpers shared with the parser core.
*/

use hardy_cbor::decode::Error as CborError;

use crate::{Error, Result, crc};

/// Synchronous continuation that carries an oversized payload block's CRC and
/// termination checks across the streamed tail. Handed back in
/// [`ParserProgress::Partial`], already fed the block header and the body
/// prefix that were in `consumed`. The caller pushes each subsequent run of
/// bytes through [`push`](Self::push); the continuation feeds the running CRC
/// (entering the CRC-value field as zeros per RFC 9171 §4.2.2), validates the
/// block-level and outer `0xFF` breaks, verifies the CRC, and reports when the
/// bundle is complete. It performs no I/O and owns no storage — persisting the
/// drained bytes is the caller's job.
///
/// [`ParserProgress::Partial`]: crate::parser::ParserProgress::Partial
pub struct PayloadTail {
    /// `None` when the payload block declared no CRC; otherwise pre-fed the
    /// block header + body prefix, and consumed by `verify_crc`.
    digest: Option<crc::Digest>,
    crc_type: crc::CrcType,
    is_indefinite: bool,
    phase: TailPhase,
    /// Body bytes not yet seen (decrements through the `Body` phase).
    body_remaining: u64,
    /// Total bytes still expected, through the outer `0xFF` break.
    remaining: u64,
    /// Captured wire CRC value bytes (`crc_value[..crc_value_len]`).
    crc_value: [u8; 4],
    crc_filled: usize,
}

/// Where in the post-`consumed` byte stream a [`PayloadTail`] currently is.
enum TailPhase {
    /// Consuming the rest of the payload body (fed to the digest).
    Body,
    /// Expecting the 1-byte CRC byte-string head (`0x42`/`0x44`).
    CrcHead,
    /// Capturing the CRC value bytes (not fed to the digest — zeros were).
    CrcValue,
    /// Expecting the block array's `0xFF` break (indefinite-length blocks only).
    BlockBreak,
    /// Expecting the bundle's outer `0xFF` break.
    OuterBreak,
    /// Bundle complete; any further bytes are trailing data.
    Done,
}

/// Wire width of the CRC value for `crc_type` (0 if none).
fn crc_value_len(crc_type: crc::CrcType) -> usize {
    match crc_type {
        crc::CrcType::CRC16_X25 => 2,
        crc::CrcType::CRC32_CASTAGNOLI => 4,
        _ => 0,
    }
}

/// CBOR byte-string head for the CRC value (`0x42` for CRC-16, `0x44` for
/// CRC-32). Only meaningful — and only consulted — when a CRC is present.
fn crc_head_byte(crc_type: crc::CrcType) -> u8 {
    match crc_type {
        crc::CrcType::CRC16_X25 => 0x42,
        crc::CrcType::CRC32_CASTAGNOLI => 0x44,
        _ => 0,
    }
}

/// The phase that follows the payload body, given the trailer shape.
fn after_body(crc_type: crc::CrcType, is_indefinite: bool) -> TailPhase {
    if !matches!(crc_type, crc::CrcType::None) {
        TailPhase::CrcHead
    } else if is_indefinite {
        TailPhase::BlockBreak
    } else {
        TailPhase::OuterBreak
    }
}

impl PayloadTail {
    pub(super) fn new(
        digest: Option<crc::Digest>,
        crc_type: crc::CrcType,
        is_indefinite: bool,
        body_remaining: u64,
        remaining: u64,
    ) -> Self {
        let phase = if body_remaining > 0 {
            TailPhase::Body
        } else {
            after_body(crc_type, is_indefinite)
        };
        Self {
            digest,
            crc_type,
            is_indefinite,
            phase,
            body_remaining,
            remaining,
            crc_value: [0; 4],
            crc_filled: 0,
        }
    }

    /// Bytes still expected before the bundle is complete (through the outer
    /// `0xFF` break).
    pub fn remaining(&self) -> u64 {
        self.remaining
    }

    /// Feed the next run of streamed bytes. Returns `true` once the bundle is
    /// complete (body drained, CRC verified, breaks consumed). Errors on a CRC
    /// mismatch ([`crc::Error::IncorrectCrc`]), a malformed trailer
    /// ([`Error::NotCanonical`]), or bytes after the outer break
    /// ([`Error::AdditionalData`]). On `Ok`, the whole run belonged to the
    /// bundle and should be persisted by the caller.
    pub fn push(&mut self, mut bytes: &[u8]) -> Result<bool> {
        let start = bytes.len();
        while let Some(&b) = bytes.first() {
            match self.phase {
                TailPhase::Body => {
                    // `Body` is only entered with `body_remaining > 0`, so this
                    // consumes at least one byte and makes progress.
                    let take = self.body_remaining.min(bytes.len() as u64) as usize;
                    if let Some(d) = self.digest.as_mut() {
                        d.push(&bytes[..take]);
                    }
                    self.body_remaining -= take as u64;
                    bytes = &bytes[take..];
                    if self.body_remaining == 0 {
                        self.enter_after_body()?;
                    }
                }
                TailPhase::CrcHead => {
                    if b != crc_head_byte(self.crc_type) {
                        return Err(Error::NotCanonical);
                    }
                    if let Some(d) = self.digest.as_mut() {
                        // The head byte is CRC input; the value field that
                        // follows is hashed as zeros (RFC 9171 §4.2.2).
                        d.push(&[b]);
                        d.push_zeros();
                    }
                    bytes = &bytes[1..];
                    self.phase = TailPhase::CrcValue;
                }
                TailPhase::CrcValue => {
                    let want = crc_value_len(self.crc_type) - self.crc_filled;
                    let take = want.min(bytes.len());
                    self.crc_value[self.crc_filled..self.crc_filled + take]
                        .copy_from_slice(&bytes[..take]);
                    self.crc_filled += take;
                    bytes = &bytes[take..];
                    if self.crc_filled == crc_value_len(self.crc_type) {
                        if self.is_indefinite {
                            self.phase = TailPhase::BlockBreak;
                        } else {
                            self.verify_crc()?;
                            self.phase = TailPhase::OuterBreak;
                        }
                    }
                }
                TailPhase::BlockBreak => {
                    if b != 0xFF {
                        return Err(Error::NotCanonical);
                    }
                    if let Some(d) = self.digest.as_mut() {
                        d.push(&[0xFF]);
                    }
                    bytes = &bytes[1..];
                    self.verify_crc()?;
                    self.phase = TailPhase::OuterBreak;
                }
                TailPhase::OuterBreak => {
                    if b != 0xFF {
                        return Err(Error::NotCanonical);
                    }
                    bytes = &bytes[1..];
                    self.phase = TailPhase::Done;
                }
                TailPhase::Done => return Err(Error::AdditionalData),
            }
        }
        self.remaining = self.remaining.saturating_sub((start - bytes.len()) as u64);
        Ok(matches!(self.phase, TailPhase::Done))
    }

    /// Assert the bundle completed. Errors with `NeedMoreData` (the still-
    /// outstanding count) if the stream ended before the outer break — i.e. the
    /// bundle was truncated.
    pub fn finish(self) -> Result<()> {
        if matches!(self.phase, TailPhase::Done) {
            Ok(())
        } else {
            Err(Error::InvalidCBOR(CborError::NeedMoreData(
                usize::try_from(self.remaining).unwrap_or(usize::MAX),
            )))
        }
    }

    /// Transition out of the `Body` phase, running the CRC verification eagerly
    /// when the next thing expected is the outer break (no CRC / no block break
    /// between here and it).
    fn enter_after_body(&mut self) -> Result<()> {
        self.phase = after_body(self.crc_type, self.is_indefinite);
        if matches!(self.phase, TailPhase::OuterBreak) {
            self.verify_crc()?;
        }
        Ok(())
    }

    /// Compare the accumulated digest against the captured wire value. A no-op
    /// when the block declared no CRC. Consumes the digest so it runs once.
    fn verify_crc(&mut self) -> Result<()> {
        if let Some(digest) = self.digest.take()
            && !digest.verify(&self.crc_value[..crc_value_len(self.crc_type)])
        {
            return Err(crc::Error::IncorrectCrc.into());
        }
        Ok(())
    }
}

/// Wire-form length of a block's post-body trailer: optional CRC byte
/// string (head byte 0x42 / 0x44 + 2 / 4 value bytes, §4.2.2) and an
/// optional `0xFF` break if the block array used indefinite-length
/// encoding.
pub(super) fn trailer_byte_len(crc_type: crc::CrcType, is_indefinite_array: bool) -> usize {
    let crc_len = match crc_type {
        crc::CrcType::None => 0,
        crc::CrcType::CRC16_X25 => 3,
        crc::CrcType::CRC32_CASTAGNOLI => 5,
        crc::CrcType::Unrecognised(_) => 0,
    };
    crc_len + if is_indefinite_array { 1 } else { 0 }
}

/// Consume the bytes after a block's body: the CRC byte string (if
/// declared) and any trailing `0xFF` break for indefinite-length block
/// arrays. Strict canonical layout per §4.2.2 / §4.3.2 makes the CRC
/// head byte exact (`0x42` for CRC-16, `0x44` for CRC-32) and the
/// value length fixed (2 or 4), so this reduces to byte matching plus
/// arithmetic. Returns the new cursor position plus the absolute start
/// offset of the CRC value bytes (if any), for the caller to run
/// `Digest::finalize` over the now-known full block extent.
pub(super) fn try_consume_block_after_body(
    data: &[u8],
    mut offset: usize,
    crc_type: crc::CrcType,
    is_indefinite_array: bool,
) -> Result<(usize, Option<usize>)> {
    let crc_value_start = match crc_type {
        crc::CrcType::None => None,
        crc::CrcType::CRC16_X25 => Some(consume_crc(data, &mut offset, 0x42, 2)?),
        crc::CrcType::CRC32_CASTAGNOLI => Some(consume_crc(data, &mut offset, 0x44, 4)?),
        crc::CrcType::Unrecognised(t) => return Err(crc::Error::InvalidType(t).into()),
    };

    if is_indefinite_array {
        match data.get(offset) {
            Some(&0xFF) => offset += 1,
            Some(_) => return Err(Error::NotCanonical),
            None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
        }
    }

    Ok((offset, crc_value_start))
}

/// Strict-shape CRC byte-string consumer: expects `head` (0x42 or 0x44)
/// at `data[*offset]`, followed by exactly `value_len` value bytes.
/// Returns the absolute offset of the first CRC value byte and advances
/// `*offset` past the CRC. `NeedMoreData` is reported for the exact
/// shortfall so the caller can wait one chunk; any other shape is a
/// canonical-encoding violation.
fn consume_crc(data: &[u8], offset: &mut usize, head: u8, value_len: usize) -> Result<usize> {
    let needed = 1 + value_len;
    if data.len() < *offset + needed {
        return Err(Error::InvalidCBOR(CborError::NeedMoreData(
            *offset + needed - data.len(),
        )));
    }
    if data[*offset] != head {
        return Err(Error::NotCanonical);
    }
    let value_start = *offset + 1;
    *offset += needed;
    Ok(value_start)
}
