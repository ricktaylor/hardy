//! Hint items (Section 7.2): typed, length-prefixed values that may lead
//! a message's content.

use alloc::vec::{self, Vec};
use core::ops::Deref;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::codec::{Error, Result};

/// Size of a single hint item header (type+H byte, length byte).
pub const HINT_HEADER_SIZE: usize = 2;

/// Maximum hint value length representable by the 8-bit length field.
pub const MAX_HINT_VALUE_LEN: usize = u8::MAX as usize;

/// Number of distinct hint types the 7-bit type field can express, and so
/// the most items [`decode_hints`] ever returns for one message.
pub(crate) const HINT_TYPE_COUNT: usize = HintType::MAX.0 as usize + 1;

/// A hint type: a value of the 7-bit Hint Type field (Section 12.2
/// registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HintType(u8);

impl HintType {
    /// The Bundle Length hint (Section 9.1).
    pub const BUNDLE_LENGTH: Self = Self(0);

    /// The largest hint type: the type occupies the upper 7 bits of the
    /// first hint header byte.
    pub const MAX: Self = Self(0x7F);

    /// Returns the hint type `value`, or `None` if it exceeds
    /// [`Self::MAX`].
    pub const fn new(value: u8) -> Option<Self> {
        if value <= Self::MAX.0 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The type code.
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// A hint value: at most [`MAX_HINT_VALUE_LEN`] bytes, what the 8-bit
/// Hint Length field can declare (Section 7.2).
///
/// Dereferences to the value's bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HintValue(Bytes);

impl HintValue {
    /// Returns the hint value `bytes`, or `None` if it is longer than
    /// [`MAX_HINT_VALUE_LEN`].
    pub fn new(bytes: Bytes) -> Option<Self> {
        (bytes.len() <= MAX_HINT_VALUE_LEN).then_some(Self(bytes))
    }

    /// The value's bytes.
    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    /// A copy of the value in an allocation of its own, so holding it does
    /// not keep alive the buffer it was decoded from.
    pub(crate) fn detached(&self) -> Self {
        Self(Bytes::copy_from_slice(&self.0))
    }

    /// The Hint Length field for this value.
    fn len_byte(&self) -> u8 {
        // At most MAX_HINT_VALUE_LEN by construction.
        self.0.len() as u8
    }
}

impl Deref for HintValue {
    type Target = Bytes;

    fn deref(&self) -> &Bytes {
        &self.0
    }
}

/// A decoded BTP-U hint item.
///
/// Every value of this type is encodable: the type and value newtypes hold
/// the field limits.
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
        /// The hint type.
        hint_type: HintType,
        /// The raw value bytes.
        value: HintValue,
    },
}

impl HintItem {
    /// The item's wire type code (Section 12.2 registry).
    pub fn hint_type(&self) -> HintType {
        match self {
            HintItem::BundleLength(_) => HintType::BUNDLE_LENGTH,
            HintItem::Unknown { hint_type, .. } => *hint_type,
        }
    }
}

/// A transfer's hints, or one message's: at most one item per hint type
/// (Section 7.2: hints are transfer-scoped and repeatable, and the value
/// most recently received supersedes an earlier one of the same type).
///
/// [`ReceiverEvent::Received`](crate::receiver::ReceiverEvent::Received)
/// delivers one, and [`SendOptions`](crate::sender::SendOptions) takes one,
/// so a relay can pass a received set straight back to a sender.
///
/// Inserting an item replaces any item of the same type.  A well-formed
/// Bundle Length is held inline, so a set holding only that does not
/// allocate.  A type-0 item whose value length Section 9.1 does not allow is
/// an [`HintItem::Unknown`] of type [`HintType::BUNDLE_LENGTH`], and it and
/// a [`HintItem::BundleLength`] replace each other like any two items of
/// one type.
///
/// Items iterate in ascending hint-type order.  Equality and hashing follow
/// the items, which have one representation per set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Hints {
    bundle_length: Option<u64>,
    /// Every other item, sorted by hint type, one entry per type: at most
    /// 128 entries of at most 255 value bytes each.
    other: Vec<(HintType, HintValue)>,
}

impl Hints {
    /// An empty set.
    pub const fn new() -> Self {
        Self {
            bundle_length: None,
            other: Vec::new(),
        }
    }

    /// Add `item`, replacing any item of the same type.
    pub fn insert(&mut self, item: HintItem) {
        let slot = self
            .other
            .binary_search_by_key(&item.hint_type(), |&(t, _)| t);
        match item {
            HintItem::BundleLength(len) => {
                self.bundle_length = Some(len);
                if let Ok(i) = slot {
                    self.other.remove(i);
                }
            }
            HintItem::Unknown { hint_type, value } => {
                if hint_type == HintType::BUNDLE_LENGTH {
                    self.bundle_length = None;
                }
                match slot {
                    Ok(i) => self.other[i].1 = value,
                    Err(i) => self.other.insert(i, (hint_type, value)),
                }
            }
        }
    }

    /// Remove and return the item of type `hint_type`, if any.
    pub fn remove(&mut self, hint_type: HintType) -> Option<HintItem> {
        if hint_type == HintType::BUNDLE_LENGTH
            && let Some(len) = self.bundle_length.take()
        {
            return Some(HintItem::BundleLength(len));
        }
        let i = self
            .other
            .binary_search_by_key(&hint_type, |&(t, _)| t)
            .ok()?;
        let (hint_type, value) = self.other.remove(i);
        Some(HintItem::Unknown { hint_type, value })
    }

    /// The well-formed Bundle Length hint's value, if the set holds one.
    pub fn bundle_length(&self) -> Option<u64> {
        self.bundle_length
    }

    /// The item of type `hint_type`, if any.  A clone: an unknown item's
    /// value is a reference-counted [`Bytes`].
    pub fn get(&self, hint_type: HintType) -> Option<HintItem> {
        if hint_type == HintType::BUNDLE_LENGTH
            && let Some(len) = self.bundle_length
        {
            return Some(HintItem::BundleLength(len));
        }
        let i = self
            .other
            .binary_search_by_key(&hint_type, |&(t, _)| t)
            .ok()?;
        let (hint_type, value) = &self.other[i];
        Some(HintItem::Unknown {
            hint_type: *hint_type,
            value: value.clone(),
        })
    }

    /// The number of items.
    pub fn len(&self) -> usize {
        usize::from(self.bundle_length.is_some()) + self.other.len()
    }

    /// Whether the set holds no items.
    pub fn is_empty(&self) -> bool {
        self.bundle_length.is_none() && self.other.is_empty()
    }

    /// The items in ascending hint-type order, cloned as for [`Self::get`].
    pub fn iter(&self) -> impl Iterator<Item = HintItem> + '_ {
        self.bundle_length
            .map(HintItem::BundleLength)
            .into_iter()
            .chain(
                self.other
                    .iter()
                    .map(|(hint_type, value)| HintItem::Unknown {
                        hint_type: *hint_type,
                        value: value.clone(),
                    }),
            )
    }

    /// The encoded size of the items (headers and values).
    pub fn encoded_len(&self) -> usize {
        self.bundle_length.map_or(0, |len| {
            HINT_HEADER_SIZE + usize::from(bundle_length_width(len))
        }) + self
            .other
            .iter()
            .map(|(_, value)| HINT_HEADER_SIZE + value.len())
            .sum::<usize>()
    }

    /// The values held as [`HintItem::Unknown`], which the receiver charges
    /// against its limits.
    pub(crate) fn unknown_values(&self) -> impl Iterator<Item = &HintValue> {
        self.other.iter().map(|(_, value)| value)
    }

    /// The items in ascending hint-type order, as a message carries them.
    pub fn into_vec(self) -> Vec<HintItem> {
        let mut items = Vec::with_capacity(self.len());
        items.extend(self.bundle_length.map(HintItem::BundleLength));
        items.extend(
            self.other
                .into_iter()
                .map(|(hint_type, value)| HintItem::Unknown { hint_type, value }),
        );
        items
    }
}

impl Extend<HintItem> for Hints {
    fn extend<I: IntoIterator<Item = HintItem>>(&mut self, items: I) {
        for item in items {
            self.insert(item);
        }
    }
}

impl FromIterator<HintItem> for Hints {
    fn from_iter<I: IntoIterator<Item = HintItem>>(items: I) -> Self {
        let mut hints = Self::new();
        hints.extend(items);
        hints
    }
}

impl From<Vec<HintItem>> for Hints {
    fn from(items: Vec<HintItem>) -> Self {
        items.into_iter().collect()
    }
}

impl IntoIterator for Hints {
    type Item = HintItem;
    type IntoIter = vec::IntoIter<HintItem>;

    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

/// Returns the total encoded size of a slice of hint items (headers + values).
pub fn encoded_hints_len(hints: &[HintItem]) -> usize {
    hints
        .iter()
        .map(|h| HINT_HEADER_SIZE + hint_value_len(h))
        .sum()
}

/// Encode a chain of hint items into `dst`.
///
/// Sets the H flag on all items except the last, per Section 7.2.
pub fn encode_hints(hints: &[HintItem], dst: &mut BytesMut) {
    let count = hints.len();
    for (i, item) in hints.iter().enumerate() {
        let more = i + 1 < count;
        write_hint(item, more, dst);
    }
}

fn write_hint(item: &HintItem, more: bool, dst: &mut BytesMut) {
    let h_bit = u8::from(more);
    match item {
        HintItem::BundleLength(len) => {
            let width = bundle_length_width(*len);
            dst.put_u8((HintType::BUNDLE_LENGTH.0 << 1) | h_bit);
            dst.put_u8(width);
            dst.put_uint(*len, usize::from(width));
        }
        HintItem::Unknown { hint_type, value } => {
            dst.put_u8((hint_type.0 << 1) | h_bit);
            dst.put_u8(value.len_byte());
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
/// message length.  The order is not normalised here; collecting the items
/// into [`Hints`] orders them by type.
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
        let hint_type = HintType(type_h_byte >> 1);
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
        match slot[usize::from(hint_type.0)] {
            NONE => {
                // Fewer than 128 distinct types can exist, so the index fits.
                slot[usize::from(hint_type.0)] = items.len() as u8;
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

fn decode_hint_item(hint_type: HintType, value: Bytes) -> HintItem {
    if hint_type == HintType::BUNDLE_LENGTH {
        // Section 9.1: the value length MUST be 1, 2, 4, or 8.  Any other
        // length is a sender fault, but the item is still fully framed, so
        // it is carried as an unknown item instead of failing the message
        // (hints are ignorable by definition, Section 7.3).
        if let 1 | 2 | 4 | 8 = value.len() {
            return HintItem::BundleLength(value.as_ref().get_uint(value.len()));
        }
    }
    HintItem::Unknown {
        hint_type,
        // The length came from an 8-bit field.
        value: HintValue(value),
    }
}
