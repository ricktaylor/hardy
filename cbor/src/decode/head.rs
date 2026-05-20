use super::*;
use smallvec::SmallVec;

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
    /// An unassigned simple value (CBOR simple values 0–19, 24–31).
    Simple(u8),
    /// A floating-point value (CBOR major type 7).
    Float(f64),
    /// An 'break' marker for indefinite length items
    Break,
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
    /// see [`Tags`] for details.
    pub tags: Tags,
    /// The decoded marker for the item itself.
    pub marker: Marker,
}

impl core::fmt::Display for Head {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let prefix = if self.tags.is_empty() {
            "Untagged"
        } else {
            "Tagged"
        };
        match self.marker {
            Marker::UnsignedInteger(_) => write!(f, "{prefix} Unsigned Integer"),
            Marker::NegativeInteger(_) => write!(f, "{prefix} Negative Integer"),
            Marker::Bytes(Some(_)) => write!(f, "{prefix} Definite-length Byte String"),
            Marker::Bytes(None) => write!(f, "{prefix} Indefinite-length Byte String"),
            Marker::Text(Some(_)) => write!(f, "{prefix} Definite-length Text String"),
            Marker::Text(None) => write!(f, "{prefix} Indefinite-length Text String"),
            Marker::Array(Some(_)) => write!(f, "{prefix} Definite-length Array"),
            Marker::Array(None) => write!(f, "{prefix} Indefinite-length Array"),
            Marker::Map(Some(_)) => write!(f, "{prefix} Definite-length Map"),
            Marker::Map(None) => write!(f, "{prefix} Indefinite-length Map"),
            Marker::False => write!(f, "{prefix} False"),
            Marker::True => write!(f, "{prefix} True"),
            Marker::Null => write!(f, "{prefix} Null"),
            Marker::Undefined => write!(f, "{prefix} Undefined"),
            Marker::Simple(v) => write!(f, "{prefix} Simple Value {v}"),
            Marker::Float(_) => write!(f, "{prefix} Float"),
            Marker::Break => write!(f, "End of sequence marker"),
        }
    }
}

impl FromCbor for Head {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let mut tags = Tags::new();
        let (mut shortest, mut offset) = parse_tags(data, &mut tags)?;
        let Some(marker) = data.get(offset) else {
            return Err(Error::NeedMoreData(1));
        };
        offset += 1;
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
                        core::num::FpCategory::Nan
                        | core::num::FpCategory::Infinite
                        | core::num::FpCategory::Zero => {
                            // There is an FP16 representation that is shorter
                            shortest = false;
                        }
                        core::num::FpCategory::Subnormal | core::num::FpCategory::Normal => {
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
                        core::num::FpCategory::Nan
                        | core::num::FpCategory::Infinite
                        | core::num::FpCategory::Zero => {
                            // There is an FP16 representation that is shorter
                            shortest = false;
                        }
                        core::num::FpCategory::Subnormal | core::num::FpCategory::Normal => {
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
            // A break code is only a valid End marker when untagged.
            // `offset == 1` means no tag bytes preceded it (offset = tag
            // bytes consumed + 1 for the marker byte). A tagged break
            // falls through to the catch-all and is rejected.
            (7, 31) if offset == 1 => (Marker::Break, true, 0),
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

    while let Some(marker) = data.get(offset) {
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
    }
    Ok((shortest, offset))
}

#[inline]
fn to_array<const N: usize>(data: &[u8]) -> Result<[u8; N], Error> {
    match data.len().cmp(&N) {
        core::cmp::Ordering::Less => Err(Error::NeedMoreData(N - data.len())),
        core::cmp::Ordering::Equal => Ok(data.try_into().unwrap()),
        core::cmp::Ordering::Greater => Ok(data[0..N].try_into().unwrap()),
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
