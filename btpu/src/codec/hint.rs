use alloc::vec::Vec;

use bytes::{BufMut, Bytes, BytesMut};

use crate::codec::{Error, Result};

/// Size of a single hint item header (type+H byte, length byte).
pub const HINT_HEADER_SIZE: usize = 2;

/// Hint type value of the Bundle Length hint (Section 9.1).
pub const BUNDLE_LENGTH_HINT: u8 = 0;

/// Maximum hint type value: the type occupies the upper 7 bits of the first
/// hint header byte.
pub const MAX_HINT_TYPE: u8 = 0x7F;

/// Maximum hint value length representable by the 8-bit length field.
pub const MAX_HINT_VALUE_LEN: usize = u8::MAX as usize;

/// Number of distinct hint types the 7-bit type field can express, and so
/// the most items [`decode_hints`] ever returns for one message.
const HINT_TYPE_COUNT: usize = MAX_HINT_TYPE as usize + 1;

/// Why a caller-constructed hint item cannot be encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// A hint type exceeds the 7-bit maximum.
    #[error("Invalid hint type {0:#04x} (must be <= 0x7f)")]
    InvalidHintType(u8),

    /// A hint value exceeds the 255-byte maximum of the 8-bit length field.
    #[error("Hint value length {length} exceeds maximum {max}")]
    ValueOverflow { length: usize, max: usize },
}

/// A decoded BTP-U hint item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintItem {
    /// Bundle Length hint: total length of the bundle being transferred
    /// (Section 9.1).
    BundleLength(u64),
    /// A hint this implementation does not interpret, preserved for forward
    /// compatibility (Section 7.3: skipped via its length, never an error).
    /// Also produced for a Bundle Length hint whose value length is not one
    /// the draft allows: the item is still fully framed, so it is carried
    /// opaquely rather than failing the message.
    Unknown {
        /// The 7-bit hint type (Section 12.2 registry).
        hint_type: u8,
        /// The raw value bytes.
        value: Bytes,
    },
}

impl HintItem {
    /// The item's wire type code (Section 12.2 registry).
    pub fn hint_type(&self) -> u8 {
        match self {
            HintItem::BundleLength(_) => BUNDLE_LENGTH_HINT,
            HintItem::Unknown { hint_type, .. } => *hint_type,
        }
    }
}

/// Returns the total encoded size of a slice of hint items (headers + values).
pub fn encoded_hints_len(hints: &[HintItem]) -> usize {
    hints
        .iter()
        .map(|h| HINT_HEADER_SIZE + hint_value_len(h))
        .sum()
}

/// Check that every hint item is encodable.
///
/// Errors if an [`HintItem::Unknown`] has a type above [`MAX_HINT_TYPE`] or
/// a value longer than [`MAX_HINT_VALUE_LEN`]; both would otherwise be
/// silently truncated into a corrupt hint chain.
// Spelled out: this module's imported `Result` is the codec alias.
pub fn validate_hints(hints: &[HintItem]) -> core::result::Result<(), ValidationError> {
    for item in hints {
        if let HintItem::Unknown { hint_type, value } = item {
            if *hint_type > MAX_HINT_TYPE {
                return Err(ValidationError::InvalidHintType(*hint_type));
            }
            if value.len() > MAX_HINT_VALUE_LEN {
                return Err(ValidationError::ValueOverflow {
                    length: value.len(),
                    max: MAX_HINT_VALUE_LEN,
                });
            }
        }
    }
    Ok(())
}

/// Encode a chain of hint items into `dst`.
///
/// Sets the H flag on all items except the last, per Section 7.2.
///
/// Errors per [`validate_hints`]; `dst` is untouched on error.
pub fn encode_hints(hints: &[HintItem], dst: &mut BytesMut) -> Result<()> {
    validate_hints(hints)?;
    write_hints(hints, dst);
    Ok(())
}

/// Write an already-validated chain of hint items into `dst`.
///
/// Message encoders validate once, before writing any header bytes, and
/// then call this rather than [`encode_hints`] so the items are not checked
/// a second time.
pub(crate) fn write_hints(hints: &[HintItem], dst: &mut BytesMut) {
    let count = hints.len();
    for (i, item) in hints.iter().enumerate() {
        let more = i + 1 < count;
        write_hint(item, more, dst);
    }
}

fn write_hint(item: &HintItem, more: bool, dst: &mut BytesMut) {
    let h_bit: u8 = if more { 1 } else { 0 };
    match item {
        HintItem::BundleLength(len) => {
            let width = bundle_length_width(*len);
            let bytes = len.to_be_bytes();
            dst.put_u8((BUNDLE_LENGTH_HINT << 1) | h_bit);
            dst.put_u8(width);
            dst.put_slice(&bytes[size_of::<u64>() - usize::from(width)..]);
        }
        HintItem::Unknown { hint_type, value } => {
            dst.put_u8((hint_type << 1) | h_bit);
            dst.put_u8(value.len() as u8);
            dst.put_slice(value);
        }
    }
}

/// The width of the shortest Bundle Length encoding that holds `len`
/// (Section 9.1: 1, 2, 4, or 8 bytes).
fn bundle_length_width(len: u64) -> u8 {
    match len {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        0x1_0000..=0xFFFF_FFFF => 4,
        _ => 8,
    }
}

fn hint_value_len(item: &HintItem) -> usize {
    match item {
        // Sized by the encoder itself so the two can never disagree.
        HintItem::BundleLength(len) => usize::from(bundle_length_width(*len)),
        HintItem::Unknown { value, .. } => value.len(),
    }
}

/// Decode the chain of hint items at the start of `content`.
///
/// Returns the decoded items and the number of bytes the chain occupied.
/// Unknown hint values are zero-copy [`Bytes`] views into `content`.
///
/// The result holds at most one item per hint type, in order of first
/// appearance on the wire, with a later repeat of a type replacing the
/// earlier value.  Section 7.2 permits repeats, and a chain may hold up to
/// half a million two-byte items, so folding while decoding keeps the
/// returned `Vec` bounded by the 128-value type space rather than by the
/// message length.  The order is not normalised here; the receiver sorts
/// by hint type when it delivers hints in
/// [`ReceiverEvent::BundleReceived`](crate::receiver::ReceiverEvent::BundleReceived).
///
/// Errors with [`Error::InsufficientData`] if the chain runs past the end of
/// `content`.
pub fn decode_hints(content: &Bytes) -> Result<(Vec<HintItem>, usize)> {
    let mut items: Vec<HintItem> = Vec::new();
    // Index into `items` of the entry for each hint type, or NONE.
    const NONE: u8 = u8::MAX;
    let mut slot = [NONE; HINT_TYPE_COUNT];
    let mut offset = 0;

    loop {
        if offset + HINT_HEADER_SIZE > content.len() {
            return Err(Error::InsufficientData {
                needed: offset + HINT_HEADER_SIZE,
                available: content.len(),
            });
        }

        let type_h_byte = content[offset];
        let hint_type = type_h_byte >> 1;
        let more = type_h_byte & 1 != 0;
        let value_len = usize::from(content[offset + 1]);
        offset += HINT_HEADER_SIZE;

        if offset + value_len > content.len() {
            return Err(Error::InsufficientData {
                needed: offset + value_len,
                available: content.len(),
            });
        }

        let value = content.slice(offset..offset + value_len);
        offset += value_len;

        let item = decode_hint_item(hint_type, value);
        match slot[usize::from(hint_type)] {
            NONE => {
                // Fewer than 128 distinct types can exist, so the index fits.
                slot[usize::from(hint_type)] = items.len() as u8;
                items.push(item);
            }
            i => items[usize::from(i)] = item,
        }

        if !more {
            break;
        }
    }

    Ok((items, offset))
}

fn decode_hint_item(hint_type: u8, value: Bytes) -> HintItem {
    if hint_type == BUNDLE_LENGTH_HINT {
        // Section 9.1: the value length MUST be 1, 2, 4, or 8.  Any other
        // length is a sender fault, but the item is still fully framed, so
        // it is carried as an unknown item instead of failing the message
        // (hints are ignorable by definition, Section 7.3).
        let len = match value.len() {
            1 | 2 | 4 | 8 => {
                let mut bytes = [0; size_of::<u64>()];
                bytes[size_of::<u64>() - value.len()..].copy_from_slice(&value);
                u64::from_be_bytes(bytes)
            }
            _ => return HintItem::Unknown { hint_type, value },
        };
        return HintItem::BundleLength(len);
    }
    HintItem::Unknown { hint_type, value }
}
