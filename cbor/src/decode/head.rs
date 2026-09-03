use core::{cmp::Ordering, fmt, num::FpCategory};

use smallvec::SmallVec;

use super::*;

/// Tag list carried inside a [`Head`].
///
/// Stored inline up to one tag — the overwhelmingly common case for BPv7
/// (most items untagged; the occasional CRC, BPSec, or CBOR-in-CBOR wrap
/// uses a single tag). Items with two or more tags spill to the heap.
/// Picking inline capacity 1 keeps the struct size close to `Vec<u64>`
/// while removing the per-parse allocation that the previous `Vec`
/// representation paid on every untagged item.
pub type Tags = SmallVec<[u64; 1]>;

/// The head of a single CBOR data item.
///
/// `Marker` captures the CBOR major type and the value carried directly in
/// the type marker encoding — a scalar, a definite-length payload length,
/// or an element count. It is the payload returned by the [`Head`]
/// [`FromCbor`] implementation when you only need to dispatch on type
/// without paying for a full decode.
///
/// # What this *does not* tell you
///
/// `Marker` does **not** carry the byte length of the encoded CBOR item as
/// a whole. The values inside the variants describe the item itself, not
/// its encoded size:
///
/// - [`Array(Some(count))`][Self::Array] / [`Map(Some(count))`][Self::Map]
///   carry the **element count** — for a map, the number of key-value
///   pairs. They say nothing about how many bytes the contained items
///   occupy.
/// - [`Bytes(Some(len))`][Self::Bytes] / [`Text(Some(len))`][Self::Text]
///   carry the **payload length in bytes**. The payload itself sits in
///   the buffer immediately after the marker head; it is not consumed
///   by the marker decode.
/// - The `None` variants of `Bytes`, `Text`, `Array`, and `Map` carry no
///   length information at all — the contents are indefinite-length and
///   must be walked to a break byte.
///
/// `Marker` derives [`Debug`], [`Clone`], and [`PartialEq`]. `PartialEq`
/// follows IEEE-754 semantics for [`Float`][Self::Float] (`NaN != NaN`).
///
/// # Bytes consumed
///
/// The byte count returned alongside a `Marker` by [`FromCbor`] covers
/// the encoding of the type marker itself — including any length
/// prefix — but never the variable-length payload that follows:
///
/// - **Scalars** (integers, floats, booleans, null, undefined, simple
///   values): the full encoding is consumed.
/// - **Definite-length strings** ([`Bytes(Some(_))`][Self::Bytes],
///   [`Text(Some(_))`][Self::Text]): only the head byte and length
///   prefix are consumed; the payload bytes remain in the buffer and
///   the `Some(len)` value gives their length.
/// - **Indefinite-length strings** ([`Bytes(None)`][Self::Bytes],
///   [`Text(None)`][Self::Text]): only the single head byte is consumed;
///   the chunks and the trailing break byte remain in the buffer.
/// - **Arrays and maps** ([`Array`][Self::Array], [`Map`][Self::Map],
///   either `Some` or `None`): only the head byte and (for definite
///   collections) the length prefix are consumed; the contained items
///   remain in the buffer for the caller to walk.
#[derive(Debug, Clone, PartialEq)]
pub enum Marker {
    /// An unsigned integer (CBOR major type 0).
    UnsignedInteger(u64),
    /// A negative integer (CBOR major type 1), stored as the raw value `n` where the actual value is `-1 - n`.
    NegativeInteger(u64),
    /// A byte string (CBOR major type 2). `Some(len)` is the payload
    /// length in bytes — the payload itself remains in the buffer
    /// immediately after the marker; `None` indicates an indefinite-length
    /// string whose chunks are still in the buffer awaiting parsing.
    Bytes(Option<u64>),
    /// A text string (CBOR major type 3). `Some(len)` is the payload
    /// length in bytes — the payload itself remains in the buffer
    /// immediately after the marker; `None` indicates an indefinite-length
    /// string whose chunks are still in the buffer awaiting parsing.
    Text(Option<u64>),
    /// A CBOR array (major type 4). `Some(count)` is the number of
    /// elements for definite-length arrays — not a byte length;
    /// `None` indicates an indefinite-length array whose elements are
    /// still in the buffer, terminated by a break byte.
    Array(Option<u64>),
    /// A CBOR map (major type 5). `Some(count)` is the number of
    /// key-value pairs for definite-length maps — not a byte length;
    /// `None` indicates an indefinite-length map whose pairs are still in
    /// the buffer, terminated by a break byte.
    Map(Option<u64>),
    /// The boolean value `false` (CBOR simple value 20).
    False,
    /// The boolean value `true` (CBOR simple value 21).
    True,
    /// The null value (CBOR simple value 22).
    Null,
    /// The undefined value (CBOR simple value 23).
    Undefined,
    /// An unassigned simple value (CBOR simple values 0–19 and 32–255;
    /// 24–31 are reserved and unencodable per RFC 8949 §3.3).
    Simple(u8),
    /// A floating-point value (CBOR major type 7).
    Float(f64),
}

/// A [`Marker`] preceded by zero or more CBOR semantic tags.
///
/// `Head` is the head of a tagged or untagged CBOR item produced by
/// the [`FromCbor`] implementation. It is the entry point for low-level
/// parsing, in contrast to the closure-driven [`parse_value`] family.
///
/// Use [`parse::<Head>(data)`][parse] to peek at the next item, or
/// [`parse::<(Head, bool, usize)>(data)`][parse] when you also need
/// the canonical-encoding flag and the byte count consumed by the marker
/// itself (see [`Marker`] for the consumption rules — this count is *not*
/// the size of the encoded item for arrays, maps, or indefinite-length
/// strings).
///
/// # When to use this over [`parse_value`]
///
/// - **No closure required.** Match directly on the returned [`Marker`]
///   instead of threading control flow through an `FnOnce`, which avoids
///   borrow-checker friction and lets the caller propagate any error type.
/// - **No contiguous materialisation.** [`parse_value`] eagerly collects
///   indefinite-length string chunks into a `Vec<Range<usize>>` and
///   constructs nested [`Series`] iterators for arrays and maps; with
///   `Head` the caller decides whether to walk the chunks or
///   sub-items at all, and may skip them byte-wise instead of parsing
///   them.
pub struct Head {
    /// CBOR major-type-6 tags preceding the item, in encoding order. Empty
    /// if the item is untagged. Stored inline for the common 0-1 tag case;
    /// see `Tags` for details.
    pub tags: Tags,
    /// The decoded marker for the item itself.
    pub marker: Marker,
}

impl Head {
    /// The wire-level [`ItemType`] of this head, for building
    /// [`Error::IncorrectType`][super::Error::IncorrectType] without
    /// allocating. The inherent mirror of the `From<&Head>` conversion,
    /// matching [`Value::item_type`][super::Value::item_type].
    pub fn item_type(&self) -> ItemType {
        self.into()
    }
}

impl fmt::Display for Head {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        ItemType::from(self).fmt(f)
    }
}

/// The payload-free shape of a CBOR item: its major type plus the
/// definite/indefinite-length distinction for strings, arrays, and maps.
/// Lengths, counts, and values are discarded — except the simple-value
/// number, which *is* the type. See [`ItemType`] for the tagged/untagged
/// wrapper used in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    /// An unsigned integer (CBOR major type 0).
    UnsignedInteger,
    /// A negative integer (CBOR major type 1).
    NegativeInteger,
    /// A definite-length byte string (CBOR major type 2).
    DefiniteBytes,
    /// An indefinite-length byte string (CBOR major type 2).
    IndefiniteBytes,
    /// A definite-length text string (CBOR major type 3).
    DefiniteText,
    /// An indefinite-length text string (CBOR major type 3).
    IndefiniteText,
    /// A definite-length array (CBOR major type 4).
    DefiniteArray,
    /// An indefinite-length array (CBOR major type 4).
    IndefiniteArray,
    /// A definite-length map (CBOR major type 5).
    DefiniteMap,
    /// An indefinite-length map (CBOR major type 5).
    IndefiniteMap,
    /// The boolean value `false` (CBOR simple value 20).
    False,
    /// The boolean value `true` (CBOR simple value 21).
    True,
    /// The null value (CBOR simple value 22).
    Null,
    /// The undefined value (CBOR simple value 23).
    Undefined,
    /// An unassigned simple value (CBOR simple values 0–19 and 32–255;
    /// 24–31 are reserved and unencodable per RFC 8949 §3.3).
    Simple(u8),
    /// A floating-point value (CBOR major type 7).
    Float,
}

impl fmt::Display for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsignedInteger => f.write_str("Unsigned Integer"),
            Self::NegativeInteger => f.write_str("Negative Integer"),
            Self::DefiniteBytes => f.write_str("Definite-length Byte String"),
            Self::IndefiniteBytes => f.write_str("Indefinite-length Byte String"),
            Self::DefiniteText => f.write_str("Definite-length Text String"),
            Self::IndefiniteText => f.write_str("Indefinite-length Text String"),
            Self::DefiniteArray => f.write_str("Definite-length Array"),
            Self::IndefiniteArray => f.write_str("Indefinite-length Array"),
            Self::DefiniteMap => f.write_str("Definite-length Map"),
            Self::IndefiniteMap => f.write_str("Indefinite-length Map"),
            Self::False => f.write_str("False"),
            Self::True => f.write_str("True"),
            Self::Null => f.write_str("Null"),
            Self::Undefined => f.write_str("Undefined"),
            Self::Simple(v) => write!(f, "Simple Value {v}"),
            Self::Float => f.write_str("Float"),
        }
    }
}

impl From<&Marker> for ItemKind {
    fn from(marker: &Marker) -> Self {
        match marker {
            Marker::UnsignedInteger(_) => Self::UnsignedInteger,
            Marker::NegativeInteger(_) => Self::NegativeInteger,
            Marker::Bytes(Some(_)) => Self::DefiniteBytes,
            Marker::Bytes(None) => Self::IndefiniteBytes,
            Marker::Text(Some(_)) => Self::DefiniteText,
            Marker::Text(None) => Self::IndefiniteText,
            Marker::Array(Some(_)) => Self::DefiniteArray,
            Marker::Array(None) => Self::IndefiniteArray,
            Marker::Map(Some(_)) => Self::DefiniteMap,
            Marker::Map(None) => Self::IndefiniteMap,
            Marker::False => Self::False,
            Marker::True => Self::True,
            Marker::Null => Self::Null,
            Marker::Undefined => Self::Undefined,
            Marker::Simple(v) => Self::Simple(*v),
            Marker::Float(_) => Self::Float,
        }
    }
}

/// The wire-level classification of a CBOR item: whether tags preceded it,
/// and its payload-free [`ItemKind`]. This is the "found" half of
/// [`Error::IncorrectType`][super::Error::IncorrectType] — it is `Copy` and
/// owns nothing, so constructing a type-mismatch error never allocates and
/// the message is only formatted if the error is actually displayed.
///
/// Build one from a decoded [`Head`] with [`Head::item_type`], or from a
/// [`Value`][super::Value] with [`Value::item_type`][super::Value::item_type].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemType {
    /// Whether one or more CBOR semantic tags preceded the item.
    pub tagged: bool,
    /// The shape of the item itself.
    pub kind: ItemKind,
}

impl fmt::Display for ItemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = if self.tagged { "Tagged" } else { "Untagged" };
        write!(f, "{prefix} {}", self.kind)
    }
}

impl From<&Head> for ItemType {
    fn from(head: &Head) -> Self {
        Self {
            tagged: !head.tags.is_empty(),
            kind: (&head.marker).into(),
        }
    }
}

impl FromCbor for Head {
    type Error = Error;

    // The non-generic workhorse every generic `parse` wrapper bottoms out
    // in: without the hint, cross-crate callers pay a call per field decode
    // and lose constant propagation into the major-type match they nearly
    // always perform immediately.
    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let mut tags = Tags::new();
        let (mut shortest, mut offset) = parse_tags(data, &mut tags)?;

        // Fast path: 9 bytes = 1 marker + at most 8 value bytes. All reads
        // below are bounds-check-free. Majors 0-5 are handled in full here;
        // majors 6 (unreachable post-parse_tags) and 7 (simple/float/break)
        // fall through to the slow match below for their per-minor logic.
        let marker = if let Some(&[marker, b0, b1, b2, b3, b4, b5, b6, b7]) =
            data[offset..].first_chunk::<9>()
        {
            offset += 1;
            let major = marker >> 5;
            let minor = marker & 0x1F;

            if major <= 5 {
                // Indef-length: minor 31, only valid for majors 2-5.
                // For majors 0-1 with minor 31, fall through to
                // parse_uint_minor_fast which returns InvalidMinorValue.
                if minor == 31 && major >= 2 {
                    let m = match major {
                        2 => Marker::Bytes(None),
                        3 => Marker::Text(None),
                        4 => Marker::Array(None),
                        5 => Marker::Map(None),
                        _ => unreachable!(),
                    };
                    return Ok((Head { tags, marker: m }, shortest, offset));
                }

                let (v, s, len) = parse_uint_minor_fast(minor, [b0, b1, b2, b3, b4, b5, b6, b7])?;
                let m = match major {
                    0 => Marker::UnsignedInteger(v),
                    1 => Marker::NegativeInteger(v),
                    2 => Marker::Bytes(Some(v)),
                    3 => Marker::Text(Some(v)),
                    4 => Marker::Array(Some(v)),
                    5 => Marker::Map(Some(v)),
                    _ => unreachable!(),
                };
                return Ok((Head { tags, marker: m }, shortest && s, offset + len));
            }

            // Major 6 or 7 — fall through to slow match.
            marker
        } else {
            let Some(marker) = data.get(offset) else {
                return Err(Error::NeedMoreData(1));
            };
            offset += 1;
            *marker
        };
        let data = &data[offset..];

        let (marker, shortest, len) = match (marker >> 5, marker & 0x1F) {
            (0, minor) => parse_uint_minor(minor, data)
                .map(|(v, s, len)| (Marker::UnsignedInteger(v), shortest && s, len))?,
            (1, minor) => parse_uint_minor(minor, data)
                .map(|(v, s, len)| (Marker::NegativeInteger(v), shortest && s, len))?,
            (2, 31) => (Marker::Bytes(None), shortest, 0),
            (2, minor) => {
                /* Known length byte string */
                parse_uint_minor(minor, data)
                    .map(|(v, s, len)| (Marker::Bytes(Some(v)), shortest && s, len))?
            }
            (3, 31) => {
                /* Indefinite length text string */
                (Marker::Text(None), shortest, 0)
            }
            (3, minor) => {
                /* Known length text string */
                parse_uint_minor(minor, data)
                    .map(|(v, s, len)| (Marker::Text(Some(v)), shortest && s, len))?
            }
            (4, 31) => {
                /* Indefinite length array */
                (Marker::Array(None), shortest, 0)
            }
            (4, minor) => {
                /* Known length array */
                parse_uint_minor(minor, data)
                    .map(|(v, s, len)| (Marker::Array(Some(v)), shortest && s, len))?
            }
            (5, 31) => {
                /* Indefinite length map */
                (Marker::Map(None), shortest, 0)
            }
            (5, minor) => {
                /* Known length map */
                parse_uint_minor(minor, data)
                    .map(|(v, s, len)| (Marker::Map(Some(v)), shortest && s, len))?
            }
            (6, _) => unreachable!("CBOR major type 6 (tags) consumed before dispatch"),
            (7, 20) => {
                /* False */
                (Marker::False, shortest, 0)
            }
            (7, 21) => {
                /* True */
                (Marker::True, shortest, 0)
            }
            (7, 22) => {
                /* Null */
                (Marker::Null, shortest, 0)
            }
            (7, 23) => {
                /* Undefined */
                (Marker::Undefined, shortest, 0)
            }
            (7, minor @ 0..=19) => {
                /* Unassigned simple type */
                (Marker::Simple(minor), shortest, 0)
            }
            (7, 24) => {
                /* Unassigned simple type */
                let Some(v) = data.first() else {
                    return Err(Error::NeedMoreData(1));
                };
                if *v < 32 {
                    return Err(Error::InvalidSimpleType(*v));
                }
                (Marker::Simple(*v), shortest, 1)
            }
            (7, 25) => {
                /* FP16 */
                let v = half::f16::from_be_bytes(to_array(data)?);
                (Marker::Float(v.into()), shortest, 2)
            }
            (7, 26) => {
                /* FP32 */
                let v = f32::from_be_bytes(to_array(data)?);
                if shortest {
                    match v.classify() {
                        FpCategory::Nan | FpCategory::Infinite | FpCategory::Zero => {
                            // There is an FP16 representation that is shorter
                            shortest = false;
                        }
                        FpCategory::Subnormal | FpCategory::Normal => {
                            if let Some(v16) = <half::f16 as num_traits::FromPrimitive>::from_f32(v)
                                && <half::f16 as num_traits::ToPrimitive>::to_f32(&v16) == Some(v)
                            {
                                shortest = false;
                            }
                        }
                    }
                }
                (Marker::Float(v.into()), shortest, 4)
            }
            (7, 27) => {
                /* FP64 */
                let v = f64::from_be_bytes(to_array(data)?);
                if shortest {
                    match v.classify() {
                        FpCategory::Nan | FpCategory::Infinite | FpCategory::Zero => {
                            // There is an FP16 representation that is shorter
                            shortest = false;
                        }
                        FpCategory::Subnormal | FpCategory::Normal => {
                            if let Some(v32) = f32::from_f64(v) {
                                if v32.to_f64() == Some(v) {
                                    shortest = false;
                                }
                            } else if let Some(v16) =
                                <half::f16 as num_traits::FromPrimitive>::from_f64(v)
                                && <half::f16 as num_traits::ToPrimitive>::to_f64(&v16) == Some(v)
                            {
                                shortest = false;
                            }
                        }
                    }
                }
                (Marker::Float(v), shortest, 8)
            }
            (7, minor) => {
                return Err(Error::InvalidSimpleType(minor));
            }
            _ => unreachable!("CBOR major type is 3 bits, all values 0-7 handled above"),
        };
        Ok((Head { tags, marker }, shortest, offset + len))
    }
}

fn parse_tags(data: &[u8], tags: &mut Tags) -> Result<(bool, usize), Error> {
    let mut offset = 0;
    let mut shortest = true;

    loop {
        if let Some(&[marker, b0, b1, b2, b3, b4, b5, b6, b7]) = data[offset..].first_chunk::<9>() {
            match (marker >> 5, marker & 0x1F) {
                (6, minor) => {
                    offset += 1;
                    let (tag, s, o) =
                        parse_uint_minor_fast(minor, [b0, b1, b2, b3, b4, b5, b6, b7])?;
                    tags.push(tag);
                    shortest &= s;
                    offset = offset.checked_add(o).ok_or(Error::TooBig)?;
                }
                _ => break,
            }
        } else if let Some(marker) = data.get(offset) {
            match (marker >> 5, marker & 0x1F) {
                (6, minor) => {
                    offset += 1;
                    let (tag, s, o) = parse_uint_minor(minor, &data[offset..])?;
                    tags.push(tag);
                    shortest &= s;
                    offset = offset.checked_add(o).ok_or(Error::TooBig)?;
                }
                _ => break,
            }
        } else {
            break;
        }
    }
    Ok((shortest, offset))
}

#[inline]
fn to_array<const N: usize>(data: &[u8]) -> Result<[u8; N], Error> {
    match data.len().cmp(&N) {
        Ordering::Less => Err(Error::NeedMoreData(N - data.len())),
        Ordering::Equal => Ok(data.try_into().unwrap()),
        Ordering::Greater => Ok(data[0..N].try_into().unwrap()),
    }
}

#[inline]
fn parse_uint_minor(minor: u8, data: &[u8]) -> Result<(u64, bool, usize), Error> {
    match minor {
        24 => {
            if let Some(val) = data.first() {
                Ok((*val as u64, *val > 23, 1))
            } else {
                Err(Error::NeedMoreData(1))
            }
        }
        25 => {
            let v = u16::from_be_bytes(to_array(data)?);
            Ok((v as u64, v > u8::MAX as u16, 2))
        }
        26 => {
            let v = u32::from_be_bytes(to_array(data)?);
            Ok((v as u64, v > u16::MAX as u32, 4))
        }
        27 => {
            let v = u64::from_be_bytes(to_array(data)?);
            Ok((v, v > u32::MAX as u64, 8))
        }
        val if val < 24 => Ok((val as u64, true, 0)),
        _ => Err(Error::InvalidMinorValue(minor)),
    }
}

#[inline]
fn parse_uint_minor_fast(minor: u8, data: [u8; 8]) -> Result<(u64, bool, usize), Error> {
    // All reads below are on a fixed-size array — no bounds checks.
    match minor {
        v if v < 24 => Ok((v as u64, true, 0)),
        24 => Ok((data[0] as u64, data[0] > 23, 1)),
        25 => {
            let v = u16::from_be_bytes([data[0], data[1]]);
            Ok((v as u64, v > u8::MAX as u16, 2))
        }
        26 => {
            let v = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            Ok((v as u64, v > u16::MAX as u32, 4))
        }
        27 => {
            let v = u64::from_be_bytes(data);
            Ok((v, v > u32::MAX as u64, 8))
        }
        _ => Err(Error::InvalidMinorValue(minor)),
    }
}

#[cfg(test)]
mod tests {
    use hex_literal::hex;

    use super::*;

    // parse_uint_minor_fast must agree with parse_uint_minor on value,
    // shortest flag, and byte count for every minor form.
    #[test]
    fn uint_minor_fast_matches_slow() {
        for (minor, bytes) in [
            (0u8, hex!("0000000000000000")),
            (23, hex!("0000000000000000")),
            (24, hex!("7B00000000000000")), // 123, canonical
            (24, hex!("1700000000000000")), // 23, non-canonical
            (25, hex!("0100000000000000")), // 256, canonical
            (25, hex!("00FF000000000000")), // 255, non-canonical
            (26, hex!("0001000000000000")), // 65536, canonical
            (26, hex!("0000FFFF00000000")), // 65535, non-canonical
            (27, hex!("0000000100000000")), // 2^32, canonical
            (27, hex!("00000000FFFFFFFF")), // 2^32 - 1, non-canonical
        ] {
            let fast = parse_uint_minor_fast(minor, bytes).unwrap();
            let slow = parse_uint_minor(minor, &bytes).unwrap();
            assert_eq!(fast, slow, "minor {minor} with bytes {bytes:02X?}");
        }

        // Reserved minor values are rejected by both paths.
        for minor in 28..=31 {
            assert!(matches!(
                parse_uint_minor_fast(minor, [0; 8]),
                Err(Error::InvalidMinorValue(m)) if m == minor
            ));
            assert!(matches!(
                parse_uint_minor(minor, &[0; 8]),
                Err(Error::InvalidMinorValue(m)) if m == minor
            ));
        }
    }

    // Head::from_cbor takes the bounds-check-free fast path when at least
    // 9 bytes remain, and the byte-at-a-time slow path otherwise. Both
    // must report identical markers, tags, shortest flags, and byte
    // counts. The padded buffer steers the fast path; the exact-length
    // buffer the slow path. In particular the byte count must not
    // double-count the marker byte on the fast path.
    #[test]
    fn fast_and_slow_paths_agree() {
        for encoding in [
            &hex!("00")[..],                         // 1-byte uint
            &hex!("18 7B")[..],                      // 2-byte uint (123)
            &hex!("19 01 00")[..],                   // 3-byte uint (256)
            &hex!("1A 00 01 00 00")[..],             // 5-byte uint (65536)
            &hex!("1B 00 00 00 01 00 00 00 00")[..], // 9-byte uint (2^32)
            &hex!("C1 00")[..],                      // tagged uint
            &hex!("D8 05 00")[..],                   // non-canonically tagged uint
        ] {
            let (slow_head, slow_s, slow_len) = Head::from_cbor(encoding).unwrap();

            let mut padded = encoding.to_vec();
            padded.resize(encoding.len() + 16, 0);
            let (fast_head, fast_s, fast_len) = Head::from_cbor(&padded).unwrap();

            assert_eq!(slow_head.marker, fast_head.marker);
            assert_eq!(slow_head.tags, fast_head.tags);
            assert_eq!(slow_s, fast_s);
            assert_eq!(slow_len, fast_len);
            assert_eq!(slow_len, encoding.len());
        }
    }

    // The fast path must AND the tag prefix's shortest flag into the
    // marker's: a non-canonical tag encoding clears it even when the
    // tagged value itself is canonical.
    #[test]
    fn fast_path_preserves_tags_shortest() {
        // d8 05 = tag(5) via minor 24 (non-canonical: 5 fits in the
        // immediate minor, whose canonical form is c5); 00 = uint 0.
        // Padded so the fast path fires.
        let mut data = hex!("D8 05 00").to_vec();
        data.resize(16, 0);
        let (head, s, len) = Head::from_cbor(&data).unwrap();
        assert_eq!(head.tags.as_slice(), &[5]);
        assert!(matches!(head.marker, Marker::UnsignedInteger(0)));
        assert!(
            !s,
            "non-canonical tag encoding must propagate shortest=false through fast path"
        );
        assert_eq!(len, 3);
    }
}
