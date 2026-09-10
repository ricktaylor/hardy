/*!
The per-block wire grammar: the block-array head and the five header
fields, decoded incrementally with the RFC 9171 §4.3.2 definite-byte-string
rule and the hand-rolled `#6.24` tag guard.
*/

use alloc::boxed::Box;

use hardy_cbor::decode::{Error as CborError, Head, Marker};

use crate::{
    Error,
    bundle::{BlockFlags, BlockType},
    canonical::CaptureFieldErr,
    crc,
};

use super::parse_canonical;

pub(super) struct BlockHeader {
    /// `true` if the block array uses indefinite-length encoding (a trailing
    /// `0xFF` break byte must be consumed after the CRC). `false` for the
    /// definite-length forms (`0x85` = 5 items / no CRC, `0x86` = 6 items /
    /// with CRC), whose item count is validated against `crc_type` by
    /// `from_cbor`.
    pub(super) is_indefinite: bool,
    pub(super) number: u64,
    pub(super) block_type: BlockType,
    pub(super) flags: BlockFlags,
    pub(super) crc_type: crc::CrcType,
    // Block payload relative to the start of the block
    pub(super) data_start: u64,
    pub(super) data_end: u64,
}

impl hardy_cbor::decode::FromCbor for BlockHeader {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> core::result::Result<(Self, bool, usize), Self::Error> {
        // Block array head — RFC 9171 §4.3.2: SHALL be a CBOR array with
        // 5 items (no CRC) or 6 items (with CRC); §4.1 carve-out permits
        // indefinite-length. The three legal head bytes are 0x85, 0x86, 0x9F.
        // Anything else is either a non-shortest definite-length form
        // (NotCanonical) or not an array at all. (A tag head never gets
        // this far: the caller's `parse_canonical` rejects it from the
        // first byte.)
        let (is_indefinite, mut offset) = match data.first() {
            Some(&0x85) => (false, 1),
            Some(&0x86) => (false, 1),
            Some(&0x9F) => (true, 1),
            Some(_) => return Err(slow_block_array_error(data)),
            None => return Err(Error::InvalidCBOR(CborError::NeedMoreData(1))),
        };
        let expected_count = data[0];

        let block_type: BlockType = parse_canonical(data, &mut offset, "block type")?;

        let block_number: u64 = parse_canonical(data, &mut offset, "block number")?;
        match (block_number, block_type) {
            (1, BlockType::Payload) => {}
            (0 | 1, _) | (_, BlockType::Primary | BlockType::Payload) => {
                return Err(Error::InvalidBlockNumber(block_number, block_type));
            }
            _ => {}
        }

        let flags: BlockFlags = parse_canonical(data, &mut offset, "block flags")?;

        let crc_type: crc::CrcType = parse_canonical(data, &mut offset, "block crc type")?;

        // Definite-length array item count must agree with crc_type.
        // (Indefinite arrays carry no count; the trailing-break consumer
        // enforces termination separately.)
        match (expected_count, crc_type) {
            (0x85, crc::CrcType::None) => {}
            (0x86, t) if !matches!(t, crc::CrcType::None) => {}
            (0x9F, _) => {}
            _ => return Err(Error::InvalidCBOR(CborError::AdditionalItems)),
        }

        // Block-type-specific data byte string head. RFC 9171 §4.3.2 is
        // categorical for this field: "a single definite-length CBOR byte
        // string, i.e., a CBOR byte string that is not of indefinite length"
        // — a specific override of §4.1's general "indefinite-length items
        // are not prohibited" carve-out. The `Bytes(None)` rejection below is
        // therefore deliberate and spec-backed: do not relax it to
        // accept-and-canonicalise. §4.1's separate "MAY accept ... and
        // transform it into conformant BP structure" robustness clause is
        // intentionally not exercised — a definite extent is also what makes
        // zero-copy payload ranges and sequential payload spooling possible.
        // The primary block is the opposite case: no §4.3.2-style override
        // applies there, §4.1 governs, and a non-canonical primary is
        // tolerated (see `bpsec::context::canonical_primary`). Pinned by
        // `tests/parse.rs`.
        //
        // Appendix B permits an optional `#6.24` tag (CBOR-embedded
        // content); no other tags are allowed. This is the one grammar
        // position where a tag is legal at all, so it cannot ride the
        // `Untagged`-based `parse_canonical`; the tag-run guard is
        // hand-rolled instead. The only shortest-form encoding of tag 24
        // is `D8 18` (non-shortest forms fail the `!s` check below), so
        // any other tag head — or a second consecutive tag — is rejected
        // from at most three bytes, never reading an adversarial tag run.
        // A buffer truncated inside a possible `D8 18` prefix must fall
        // through to the `Head` parse below so a streamed caller gets
        // `NeedMoreData`, not a premature `NotCanonical`.
        if let Some(first @ 0xC0..=0xDB) = data.get(offset)
            && (*first != 0xD8
                || matches!(data.get(offset + 1), Some(b) if *b != 0x18)
                || matches!(data.get(offset + 2), Some(0xC0..=0xDB)))
        {
            return Err(Error::NotCanonical);
        }
        let (marker, s, l): (Head, bool, usize) =
            hardy_cbor::decode::parse(&data[offset..]).map_field_err::<Error>("block data")?;
        if !s {
            return Err(Error::InvalidField {
                field: "block data",
                source: Box::new(Error::NotCanonical),
            });
        }
        offset = offset
            .checked_add(l)
            .ok_or(Error::InvalidCBOR(CborError::TooBig))?;
        // The guard above already narrows the tags to `[]` or `[24]`;
        // this stays as the normative statement of what is accepted.
        if !matches!(marker.tags.as_slice(), [] | [24]) {
            return Err(Error::NotCanonical);
        }
        let data_end = match marker.marker {
            Marker::Bytes(Some(len)) => len.checked_add(offset as u64).ok_or(CborError::TooBig)?,
            Marker::Bytes(None) => return Err(Error::NotCanonical),
            _ => {
                return Err(Error::InvalidCBOR(CborError::IncorrectType(
                    "Definite-length Byte String",
                    marker.item_type(),
                )))
                .map_field_err::<Error>("block data");
            }
        };

        Ok((
            Self {
                is_indefinite,
                number: block_number,
                block_type,
                flags,
                crc_type,
                data_start: offset as u64,
                data_end,
            },
            true,
            offset,
        ))
    }
}

/// Cold path: the block-array first byte wasn't `0x85`, `0x86`, or
/// `0x9F`. Re-parse via `Head` to distinguish "not an array" from
/// "definite-length array with the wrong item count" (the latter is
/// still a canonical violation, but we map it to `InvalidCBOR` for
/// diagnostic continuity with the underlying CBOR machinery). The
/// re-parse is bounded: a tag head never reaches here — the caller's
/// `parse_canonical` rejects a tag run from its first byte — and for any
/// non-tag first byte the `Head` tag scan exits immediately, so this
/// reject path stays free of attacker-driven work and allocation.
#[cold]
fn slow_block_array_error(data: &[u8]) -> Error {
    match hardy_cbor::decode::parse::<(Head, bool, usize)>(data) {
        Ok((marker, _, _)) => match marker.marker {
            Marker::Array(Some(7..)) => Error::InvalidCBOR(CborError::AdditionalItems),
            Marker::Array(Some(_)) => Error::InvalidCBOR(CborError::NoMoreItems),
            _ => Error::InvalidCBOR(CborError::IncorrectType(
                "Definite length array",
                marker.item_type(),
            )),
        },
        Err(e) => Error::InvalidCBOR(e),
    }
}
