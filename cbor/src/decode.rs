/*!
A canonical CBOR decoder for parsing byte streams.

This module provides tools for decoding data from the Concise Binary Object
Representation (CBOR) format, as specified in [RFC 8949]. The decoder is
designed to handle both simple and complex CBOR structures, including
definite and indefinite-length items, and semantic tags.

# Core Concepts

There are three primary ways to use the decoder:

1.  **Direct Deserialization with `FromCbor`:** For straightforward cases, you can
    implement the [`FromCbor`] trait for your types. This allows you to
    convert a CBOR byte slice directly into a Rust struct.

2.  **Streaming Parsing with `parse_*` functions:** For more complex or
    performance-sensitive scenarios, you can use the `parse_*` functions
    ([`parse_value`], [`parse_array`], [`parse_map`]) to process the CBOR
    stream piece by piece. This approach gives you fine-grained control and
    avoids intermediate allocations.

3.  **Low-level peek with [`TaggedMarker`]:** For optimised parsing where
    you want to dispatch on type and length without using a closure or
    materialising indefinite-length contents, parse a [`TaggedMarker`] and
    match on its [`Marker`] directly. The caller chooses whether to walk
    the contents of arrays, maps, and chunked strings.

# Usage

## 1. Implementing `FromCbor`

To deserialize a CBOR byte slice into your custom type, implement [`FromCbor`].

```
use hardy_cbor::decode::{self, FromCbor, Error};

struct Point {
    x: i32,
    y: i32,
}

impl FromCbor for Point {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        decode::parse_array(data, |a, shortest, _| {
            let x = a.parse()?;
            let y = a.parse()?;
            Ok((Point { x, y }, shortest))
        }).map(|((v, s), len)| (v, s, len))
    }
}

// CBOR for `[10, -20]`
let bytes = &[0x82, 0x0A, 0x33];
let (point, shortest, len) = Point::from_cbor(bytes).unwrap();

assert_eq!(point.x, 10);
assert_eq!(point.y, -20);
assert!(shortest);
assert_eq!(len, bytes.len());
```

## 2. Streaming Parsing

Use [`parse_value`] to inspect a CBOR item without allocating new memory
for its contents (such as strings or byte strings).

```
use hardy_cbor::decode::{self, Value};

// CBOR for `24(h'68656c6c6f')`
let bytes = &[0xd8, 0x18, 0x45, 0x68, 0x65, 0x6c, 0x6c, 0x6f];

let ((), len) = decode::parse_value(bytes, |value, shortest, tags| {
    assert_eq!(tags, &[24]); // Semantic tag 24
    assert!(matches!(value, Value::Bytes(range) if &bytes[range.clone()] == b"hello"));
    Ok::<_, decode::Error>(())
}).unwrap();

assert_eq!(len, bytes.len());
```

[RFC 8949]: https://www.rfc-editor.org/rfc/rfc8949.html
*/
use super::*;
use core::{ops::Range, str::Utf8Error};
use num_traits::{FromPrimitive, ToPrimitive};
use thiserror::Error;

/// An error that can occur during CBOR decoding.
#[derive(Error, Debug)]
pub enum Error {
    /// An encoded item's length exceeds `usize::MAX` or available memory.
    #[error("An encoded item requires more memory than available")]
    TooBig,

    /// The input data is incomplete and more bytes are needed to decode the value.
    #[error("Need at least {0} more bytes to decode value")]
    NeedMoreData(usize),

    /// The input data contains extra, unread items after a sequence has been fully parsed.
    /// This is often returned when `complete()` is called on a [`Series`] that is not at its end.
    #[error("Additional unread items in sequence")]
    AdditionalItems,

    /// An attempt was made to parse an item from a sequence that has already ended.
    #[error("No more items in sequence")]
    NoMoreItems,

    /// The CBOR item has an invalid minor type value for its major type.
    #[error("Invalid minor-type value {0}")]
    InvalidMinorValue(u8),

    /// The CBOR item's type does not match the expected type.
    #[error("Incorrect type, expecting {0}, found {1}")]
    IncorrectType(String, String),

    /// An indefinite-length string contains an invalid chunk (e.g., not a string type).
    #[error("Chunked string contains an invalid chunk")]
    InvalidChunk,

    /// A simple value was found that is unassigned or reserved.
    #[error("Invalid simple type {0}")]
    InvalidSimpleType(u8),

    /// An indefinite-length map is missing a value for a key.
    #[error("Map has key but no value")]
    PartialMap,

    /// The maximum recursion depth was reached while decoding nested structures.
    #[error("Maximum recursion depth reached")]
    MaxRecursion,

    /// A text string contains invalid UTF-8.
    #[error(transparent)]
    InvalidUtf8(#[from] Utf8Error),

    /// An integer conversion failed, typically due to an out-of-range value.
    #[error(transparent)]
    TryFromIntError(#[from] core::num::TryFromIntError),

    /// A floating-point conversion would result in a loss of precision.
    #[error("Loss of floating-point precision")]
    PrecisionLoss,
}

/// A trait for types that can be decoded from a CBOR byte slice.
///
/// This trait is the foundation of the decoding system. By implementing `FromCbor`
/// for a type, you define how it can be constructed from a CBOR representation.
/// The library provides implementations for most primitive types, `String`, `Box<[u8]>`,
/// and tuples.
pub trait FromCbor: Sized {
    /// The error type returned when decoding fails.
    type Error;

    /// Decodes an instance of the type from the beginning of a CBOR byte slice.
    ///
    /// On success, returns a tuple containing:
    /// - The decoded value.
    /// - A boolean indicating if the value was encoded in its shortest, canonical form.
    /// - The number of bytes consumed from the slice.
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error>;
}

/// A type alias for a generic, untyped CBOR sequence.
pub type Sequence<'a> = super::decode_seq::Series<'a, 0>;
/// A type alias for a [`Series`] that represents a CBOR array.
pub type Array<'a> = super::decode_seq::Series<'a, 1>;
/// A type alias for a [`Series`] that represents a CBOR map.
pub type Map<'a> = super::decode_seq::Series<'a, 2>;
/// A stateful iterator for decoding a sequence of CBOR items (e.g., an array or map).
pub use super::decode_seq::Series;

/// Represents a single, decoded CBOR data item.
pub enum Value<'a, 'b: 'a> {
    /// An unsigned integer (CBOR major type 0).
    UnsignedInteger(u64),
    /// A negative integer (CBOR major type 1), stored as the raw value `n` where the actual value is `-1 - n`.
    NegativeInteger(u64),
    /// A byte string (CBOR major type 2) as a byte range into the source buffer.
    Bytes(Range<usize>),
    /// An indefinite-length byte string (CBOR major type 2) as a sequence of chunk ranges.
    ByteStream(Vec<Range<usize>>),
    /// A text string (CBOR major type 3).
    Text(&'b str),
    /// An indefinite-length text string (CBOR major type 3) as a sequence of chunks.
    TextStream(&'a [&'b str]),
    /// A CBOR array (major type 4).
    Array(&'a mut Array<'b>),
    /// A CBOR map (major type 5).
    Map(&'a mut Map<'b>),
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
}

impl<'a, 'b: 'a> Value<'a, 'b> {
    /// Returns a human-readable string describing the type of the CBOR value.
    pub fn type_name(&self, tagged: bool) -> String {
        let prefix = if tagged { "Tagged " } else { "Untagged " }.to_string();
        match self {
            Value::UnsignedInteger(_) => prefix + "Unsigned Integer",
            Value::NegativeInteger(_) => prefix + "Negative Integer",
            Value::Bytes(_) => prefix + "Definite-length Byte String",
            Value::ByteStream(_) => prefix + "Indefinite-length Byte String",
            Value::Text(_) => prefix + "Definite-length Text String",
            Value::TextStream(_) => prefix + "Indefinite-length Text String",
            Value::Array(a) if a.is_definite() => prefix + "Definite-length Array",
            Value::Array(_) => prefix + "Indefinite-length Array",
            Value::Map(m) if m.is_definite() => prefix + "Definite-length Map",
            Value::Map(_) => prefix + "Indefinite-length Map",
            Value::False => prefix + "False",
            Value::True => prefix + "True",
            Value::Null => prefix + "Null",
            Value::Undefined => prefix + "Undefined",
            Value::Simple(v) => format!("{prefix}Simple Value {v}"),
            Value::Float(_) => prefix + "Float",
        }
    }

    /// Skips over the content of the current value.
    ///
    /// For simple types this does nothing. For arrays and maps it consumes
    /// all nested items until the end of the sequence is reached. Returns
    /// whether every nested marker was minimally encoded; indefiniteness is
    /// not considered (use [`Series::is_definite`] on the value itself if
    /// you need that signal).
    #[deprecated(
        note = "use `decode::skip_value` on the byte slice, or `Series::skip_value` inside a sequence — both avoid the chunk-list and Series allocations made by parse_value"
    )]
    pub fn skip(&mut self, mut max_recursion: usize) -> Result<bool, Error> {
        match self {
            Value::Array(a) => {
                if max_recursion == 0 {
                    return Err(Error::MaxRecursion);
                }
                max_recursion -= 1;
                a.skip_to_end(max_recursion)
            }
            Value::Map(m) => {
                if max_recursion == 0 {
                    return Err(Error::MaxRecursion);
                }
                max_recursion -= 1;
                m.skip_to_end(max_recursion)
            }
            _ => Ok(true),
        }
    }
}

impl<'a, 'b: 'a> core::fmt::Debug for Value<'a, 'b> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Value::UnsignedInteger(n) => write!(f, "{n:?}"),
            Value::NegativeInteger(n) => write!(f, "-{n:?}"),
            Value::Bytes(b) => write!(f, "bytes[{b:?}]"),
            Value::ByteStream(b) => write!(f, "byte_stream{b:?}"),
            Value::Text(s) => write!(f, "{s:?}"),
            Value::TextStream(s) => write!(f, "{s:?}"),
            Value::Array(a) => write!(f, "{a:?}"),
            Value::Map(m) => write!(f, "{m:?}"),
            Value::False => f.write_str("false"),
            Value::True => f.write_str("true"),
            Value::Null => f.write_str("null"),
            Value::Undefined => f.write_str("undefined"),
            Value::Simple(v) => write!(f, "simple value {v}"),
            Value::Float(v) => write!(f, "{v:?}"),
        }
    }
}

fn parse_tags(data: &[u8]) -> Result<(Vec<u64>, bool, usize), Error> {
    let mut tags = Vec::new();
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
    Ok((tags, shortest, offset))
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

/// The head of a single CBOR data item.
///
/// `Marker` captures the CBOR major type and the value carried directly in
/// the type marker encoding — a scalar, a definite-length payload range, or
/// an element count. It is the payload returned by the [`TaggedMarker`]
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
/// - [`Bytes(Some(range))`][Self::Bytes] /
///   [`Text(Some(range))`][Self::Text] carry a [`Range`] into the original
///   input that locates the payload bytes; this is a slice index, not a
///   length-of-encoding.
/// - The `None` variants of `Bytes`, `Text`, `Array`, and `Map` carry no
///   length information at all — the contents are indefinite-length and
///   must be walked to a break byte.
///
/// # Bytes consumed
///
/// The byte count returned alongside a `Marker` by [`FromCbor`] covers
/// only the encoding of the type marker itself, plus the validated payload
/// of definite-length strings:
///
/// - **Scalars** (integers, floats, booleans, null, undefined, simple
///   values): the full encoding is consumed.
/// - **Definite-length strings** ([`Bytes(Some(_))`][Self::Bytes],
///   [`Text(Some(_))`][Self::Text]): the head byte, length prefix, and
///   payload bytes are all consumed; the [`Range`] then locates the
///   payload within the original input.
/// - **Indefinite-length strings** ([`Bytes(None)`][Self::Bytes],
///   [`Text(None)`][Self::Text]): only the single head byte is consumed;
///   the chunks and the trailing break byte remain in the buffer.
/// - **Arrays and maps** ([`Array`][Self::Array], [`Map`][Self::Map],
///   either `Some` or `None`): only the head byte and (for definite
///   collections) the length prefix are consumed; the contained items
///   remain in the buffer for the caller to walk.
pub enum Marker {
    /// An unsigned integer (CBOR major type 0).
    UnsignedInteger(u64),
    /// A negative integer (CBOR major type 1), stored as the raw value `n` where the actual value is `-1 - n`.
    NegativeInteger(u64),
    /// A byte string (CBOR major type 2). `Some(range)` is the byte range
    /// of the payload within the original input for definite-length
    /// strings; `None` indicates an indefinite-length string whose chunks
    /// are still in the buffer awaiting parsing.
    Bytes(Option<u64>),
    /// A text string (CBOR major type 3). `Some(range)` is the byte range
    /// of the payload within the original input for definite-length
    /// strings; `None` indicates an indefinite-length string whose chunks
    /// are still in the buffer awaiting parsing.
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
    /// An 'end' marker for indefinite length items
    End,
}

/// A [`Marker`] preceded by zero or more CBOR semantic tags.
///
/// `TaggedMarker` is the head of a tagged or untagged CBOR item produced by
/// the [`FromCbor`] implementation. It is the entry point for low-level
/// parsing, in contrast to the closure-driven [`parse_value`] family.
///
/// Use [`parse::<TaggedMarker>(data)`][parse] to peek at the next item, or
/// [`parse::<(TaggedMarker, bool, usize)>(data)`][parse] when you also need
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
///   `TaggedMarker` the caller decides whether to walk the chunks or
///   sub-items at all, and may skip them byte-wise instead of parsing
///   them.
pub struct TaggedMarker {
    /// CBOR major-type-6 tags preceding the item, in encoding order. Empty
    /// if the item is untagged.
    pub tags: Vec<u64>,
    /// The decoded marker for the item itself.
    pub marker: Marker,
}

impl core::fmt::Display for TaggedMarker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let prefix = if self.tags.is_empty() {
            "Untagged"
        } else {
            "Tagged"
        }
        .to_string();
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
            Marker::End => write!(f, "End of sequence marker"),
        }
    }
}

impl FromCbor for TaggedMarker {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (tags, mut shortest, mut offset) = parse_tags(data)?;
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
            (7, 31) if offset == 1 => (Marker::End, true, 0),
            (7, minor) => {
                return Err(Error::InvalidSimpleType(minor));
            }
            _ => unreachable!("CBOR major type is 3 bits, all values 0-7 handled above"),
        };
        Ok((TaggedMarker { tags, marker }, shortest, offset + len))
    }
}

/// Skips over the next CBOR data item without materialising its contents.
///
/// Walks the encoding using [`TaggedMarker`] dispatch, recursing into
/// arrays and maps and walking the chunks of indefinite-length strings.
/// No `Vec` is allocated for chunk lists and no [`Series`] iterator is
/// constructed.
///
/// On success returns `(shortest, len)` where `len` is the total byte
/// count consumed and `shortest` reports whether every marker encountered
/// was minimally encoded (lengths, tags, and floats per [RFC 8949 §4.2.1]).
/// Indefiniteness itself does **not** clear the flag — callers that treat
/// indefinite-length items as non-canonical should pattern-match on the
/// returned `Marker` or use [`Series::is_definite`] separately.
///
/// # Errors
///
/// Returns [`Error::MaxRecursion`] if nested arrays or maps exceed
/// `max_recursion`.
///
/// [RFC 8949 §4.2.1]: https://www.rfc-editor.org/rfc/rfc8949.html#section-4.2.1
pub fn skip_value(data: &[u8], mut max_recursion: usize) -> Result<(bool, usize), Error> {
    let (marker, mut shortest, mut offset) = parse::<(TaggedMarker, bool, usize)>(data)?;
    match marker.marker {
        Marker::Bytes(Some(v)) | Marker::Text(Some(v)) => {
            Ok((shortest, checked_offset(offset, v)?))
        }
        Marker::Bytes(None) => loop {
            let (inner_marker, inner_shortest, len) =
                parse::<(TaggedMarker, bool, usize)>(&data[offset..])?;
            offset = offset.checked_add(len).ok_or(Error::TooBig)?;

            match inner_marker.marker {
                Marker::Bytes(Some(len)) if inner_marker.tags.is_empty() => {
                    offset = checked_offset(offset, len)?;
                    shortest &= inner_shortest;
                }
                Marker::End => return Ok((shortest, offset)),
                _ => return Err(Error::InvalidChunk),
            }
        },
        Marker::Text(None) => loop {
            let (inner_marker, inner_shortest, len) =
                parse::<(TaggedMarker, bool, usize)>(&data[offset..])?;
            offset = offset.checked_add(len).ok_or(Error::TooBig)?;

            match inner_marker.marker {
                Marker::Text(Some(len)) if inner_marker.tags.is_empty() => {
                    offset = checked_offset(offset, len)?;
                    shortest &= inner_shortest;
                }
                Marker::End => return Ok((shortest, offset)),
                _ => return Err(Error::InvalidChunk),
            }
        },
        Marker::Array(Some(count)) | Marker::Map(Some(count)) => {
            if max_recursion == 0 {
                return Err(Error::MaxRecursion);
            }
            max_recursion -= 1;
            for _ in 0..count {
                let (inner_shortest, len) = skip_value(&data[offset..], max_recursion)?;
                shortest &= inner_shortest;
                offset = offset.checked_add(len).ok_or(Error::TooBig)?;
            }
            Ok((shortest, offset))
        }
        Marker::Array(None) | Marker::Map(None) => {
            if max_recursion == 0 {
                return Err(Error::MaxRecursion);
            }
            max_recursion -= 1;
            while *data.get(offset).ok_or(Error::NeedMoreData(1))? != 0xFF {
                let (inner_shortest, len) = skip_value(&data[offset..], max_recursion)?;
                shortest &= inner_shortest;
                offset = offset.checked_add(len).ok_or(Error::TooBig)?;
            }
            Ok((shortest, offset + 1))
        }
        _ => Ok((shortest, offset)),
    }
}

#[inline]
fn checked_offset(offset: usize, len: u64) -> Result<usize, Error> {
    usize::try_from(len)
        .ok()
        .and_then(|len| offset.checked_add(len))
        .ok_or(Error::TooBig)
}

#[inline]
fn offset_extent(data: &[u8], offset: usize, len: u64) -> Result<Range<usize>, Error> {
    let end = checked_offset(offset, len)?;
    if end > data.len() {
        Err(Error::NeedMoreData(end - data.len()))
    } else {
        Ok(offset..end)
    }
}

/// Parses a single CBOR value from a byte slice and processes it with a closure.
///
/// Reads the next item, including any preceding semantic tags, and passes a
/// fully-formed [`Value`] to `f`. Indefinite-length strings are gathered
/// into chunk lists and arrays/maps are presented as nested [`Series`]
/// iterators, so the closure sees a uniform view regardless of encoding.
/// For finer control without a closure, parse a [`TaggedMarker`] instead.
///
/// On success, returns the result of `f` and the total number of bytes
/// consumed from the input slice.
pub fn parse_value<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(Value, bool, &[u64]) -> Result<T, E>,
    E: From<Error>,
{
    let (marker, mut shortest, mut offset) = parse::<(TaggedMarker, bool, usize)>(data)?;
    match marker.marker {
        Marker::UnsignedInteger(v) => f(Value::UnsignedInteger(v), shortest, &marker.tags),
        Marker::NegativeInteger(v) => f(Value::NegativeInteger(v), shortest, &marker.tags),
        Marker::Bytes(Some(len)) => {
            let extent = offset_extent(data, offset, len)?;
            offset = extent.end;
            f(Value::Bytes(extent), shortest, &marker.tags)
        }
        Marker::Bytes(None) => {
            let mut chunks = Vec::new();
            loop {
                let (inner_marker, inner_shortest, len) =
                    parse::<(TaggedMarker, bool, usize)>(&data[offset..])?;

                offset = offset.checked_add(len).ok_or(Error::TooBig)?;

                match inner_marker.marker {
                    Marker::Bytes(Some(len)) if inner_marker.tags.is_empty() => {
                        let extent = offset_extent(data, offset, len)?;
                        offset = extent.end;
                        chunks.push(extent);
                        shortest &= inner_shortest;
                    }
                    Marker::End => {
                        break f(Value::ByteStream(chunks), shortest, &marker.tags);
                    }
                    _ => {
                        return Err(Error::InvalidChunk.into());
                    }
                }
            }
        }
        Marker::Text(Some(len)) => {
            let extent = offset_extent(data, offset, len)?;
            offset = extent.end;
            f(
                Value::Text(core::str::from_utf8(&data[extent]).map_err(Into::into)?),
                shortest,
                &marker.tags,
            )
        }
        Marker::Text(None) => {
            let mut chunks = Vec::new();
            loop {
                let (inner_marker, inner_shortest, len) =
                    parse::<(TaggedMarker, bool, usize)>(&data[offset..])?;

                offset = offset.checked_add(len).ok_or(Error::TooBig)?;

                match inner_marker.marker {
                    Marker::Text(Some(len)) if inner_marker.tags.is_empty() => {
                        let extent = offset_extent(data, offset, len)?;
                        offset = extent.end;
                        chunks.push(core::str::from_utf8(&data[extent]).map_err(Into::into)?);
                        shortest &= inner_shortest;
                    }
                    Marker::End => {
                        break f(Value::TextStream(&chunks), shortest, &marker.tags);
                    }
                    _ => {
                        return Err(Error::InvalidChunk.into());
                    }
                }
            }
        }
        Marker::Array(count) => {
            let count = count
                .map(|c| usize::try_from(c).map_err(|_| Error::TooBig))
                .transpose()?;
            let mut a = Array::new(data, count, &mut offset);
            let r = f(Value::Array(&mut a), shortest, &marker.tags)?;
            a.complete(r).map_err(Into::into)
        }
        Marker::Map(count) => {
            let count = count
                .map(|c| {
                    usize::try_from(c)
                        .map_err(|_| Error::TooBig)
                        .and_then(|c| c.checked_mul(2).ok_or(Error::TooBig))
                })
                .transpose()?;
            let mut m = Map::new(data, count, &mut offset);
            let r = f(Value::Map(&mut m), shortest, &marker.tags)?;
            m.complete(r).map_err(Into::into)
        }
        Marker::False => f(Value::False, shortest, &marker.tags),
        Marker::True => f(Value::True, shortest, &marker.tags),
        Marker::Null => f(Value::Null, shortest, &marker.tags),
        Marker::Undefined => f(Value::Undefined, shortest, &marker.tags),
        Marker::Simple(v) => f(Value::Simple(v), shortest, &marker.tags),
        Marker::Float(v) => f(Value::Float(v), shortest, &marker.tags),
        Marker::End => return Err(Error::InvalidSimpleType(31).into()),
    }
    .map(|r| (r, offset))
}

/// Parses a generic, untyped CBOR sequence from a byte slice.
///
/// A CBOR sequence is a series of top-level data items, not enclosed in an
/// array. This function provides a [`Sequence`] iterator to the closure `f`
/// to process each item. It is useful for formats that concatenate multiple
/// CBOR objects.
pub fn parse_sequence<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Sequence) -> Result<T, E>,
    E: From<Error>,
{
    let mut offset = 0;
    let mut s = Sequence::new(data, None, &mut offset);
    let r = f(&mut s)?;
    s.complete(()).map(|_| (r, offset)).map_err(Into::into)
}

/// Parses a CBOR array from a byte slice.
///
/// This is a convenience wrapper around [`parse_value`] that ensures the next
/// item in the stream is a CBOR array. It then provides an [`Array`] iterator
/// to the closure `f` for processing the array's elements.
pub fn parse_array<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Array, bool, &[u64]) -> Result<T, E>,
    E: From<Error>,
{
    let (marker, shortest, mut offset) = parse::<(TaggedMarker, bool, usize)>(data)?;
    match marker.marker {
        Marker::Array(count) => {
            let count = count
                .map(|c| usize::try_from(c).map_err(|_| Error::TooBig))
                .transpose()?;
            let mut a = Array::new(data, count, &mut offset);
            let r = f(&mut a, shortest, &marker.tags)?;
            a.complete(r).map_err(Into::into)
        }
        _ => Err(Error::IncorrectType("Array".to_string(), marker.to_string()).into()),
    }
    .map(|r| (r, offset))
}

/// Parses a CBOR map from a byte slice.
///
/// This is a convenience wrapper around [`parse_value`] that ensures the next
/// item in the stream is a CBOR map. It then provides a [`Map`] iterator
/// to the closure `f` for processing the map's key-value pairs.
pub fn parse_map<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(&mut Map, bool, &[u64]) -> Result<T, E>,
    E: From<Error>,
{
    let (marker, shortest, mut offset) = parse::<(TaggedMarker, bool, usize)>(data)?;
    match marker.marker {
        Marker::Map(count) => {
            let count = count
                .map(|c| {
                    usize::try_from(c)
                        .map_err(|_| Error::TooBig)
                        .and_then(|c| c.checked_mul(2).ok_or(Error::TooBig))
                })
                .transpose()?;
            let mut m = Map::new(data, count, &mut offset);
            let r = f(&mut m, shortest, &marker.tags)?;
            m.complete(r).map_err(Into::into)
        }
        _ => Err(Error::IncorrectType("Map".to_string(), marker.to_string()).into()),
    }
    .map(|r| (r, offset))
}

/// A convenience function to decode a single value that implements [`FromCbor`].
///
/// This function is a shorthand for `T::from_cbor(data).map(|v| v.0)`. It
/// decodes the value and discards the `shortest` and `len` information,
/// returning only the decoded object.
#[inline]
pub fn parse<T>(data: &[u8]) -> Result<T, T::Error>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    T::from_cbor(data).map(|v| v.0)
}

macro_rules! impl_uint_from_cbor {
    ($($ty:ty),*) => {
        $(
            impl FromCbor for $ty {
                type Error = self::Error;

                #[inline]
                fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
                    let (v,shortest,len) = u64::from_cbor(data)?;
                    Ok((v.try_into()?, shortest, len))
                }
            }
        )*
    };
}

impl_uint_from_cbor!(u8, u16, u32, usize);

impl FromCbor for u64 {
    type Error = self::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(TaggedMarker, bool, usize)>(data)?;
        if let Marker::UnsignedInteger(v) = marker.marker {
            Ok((v, shortest && marker.tags.is_empty(), offset))
        } else {
            Err(Error::IncorrectType(
                "Untagged Unsigned Integer".to_string(),
                marker.to_string(),
            ))
        }
    }
}

macro_rules! impl_int_from_cbor {
    ($($ty:ty),*) => {
        $(
            impl FromCbor for $ty {
                type Error = self::Error;

                #[inline]
                fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
                    let (v,shortest,len) = i64::from_cbor(data)?;
                    Ok((v.try_into()?, shortest, len))
                }
            }
        )*
    };
}

impl_int_from_cbor!(i8, i16, i32, isize);

impl FromCbor for i64 {
    type Error = self::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(TaggedMarker, bool, usize)>(data)?;
        match marker.marker {
            Marker::UnsignedInteger(v) => Ok((
                i64::try_from(v)?,
                shortest && marker.tags.is_empty(),
                offset,
            )),
            Marker::NegativeInteger(n) => Ok((
                -1i64 - i64::try_from(n)?,
                shortest && marker.tags.is_empty(),
                offset,
            )),
            _ => Err(Error::IncorrectType(
                "Untagged Integer".to_string(),
                marker.to_string(),
            )),
        }
    }
}

macro_rules! impl_float_from_cbor {
    ($(($ty:ty, $convert_expr:expr)),*) => {
        $(
            impl FromCbor for $ty {
                type Error = self::Error;

                #[inline]
                fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
                    let (v, shortest, len) = f64::from_cbor(data)?;
                    Ok((
                        $convert_expr(v).ok_or(Error::PrecisionLoss)?,
                        shortest,
                        len,
                    ))
                }
            }
        )*
    };
}

impl_float_from_cbor!(
    (half::f16, |v: f64| {
        <half::f16 as num_traits::FromPrimitive>::from_f64(v)
    }),
    (f32, f32::from_f64)
);

impl FromCbor for f64 {
    type Error = self::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(TaggedMarker, bool, usize)>(data)?;
        if let Marker::Float(v) = marker.marker {
            Ok((v, shortest && marker.tags.is_empty(), offset))
        } else {
            Err(Error::IncorrectType(
                "Untagged Float".to_string(),
                marker.to_string(),
            ))
        }
    }
}

impl FromCbor for bool {
    type Error = self::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (marker, shortest, offset) = parse::<(TaggedMarker, bool, usize)>(data)?;
        match marker.marker {
            Marker::False => Ok((false, shortest && marker.tags.is_empty(), offset)),
            Marker::True => Ok((true, shortest && marker.tags.is_empty(), offset)),
            _ => Err(Error::IncorrectType(
                "Untagged Boolean".to_string(),
                marker.to_string(),
            )),
        }
    }
}

impl<T> FromCbor for Option<T>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

    #[inline]
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        match parse_value(data, |value, shortest, tags| match value {
            Value::Undefined => Ok(Some(shortest && tags.is_empty())),
            _ => Ok(None),
        })? {
            (Some(shortest), len) => Ok((None, shortest, len)),
            (None, _) => T::from_cbor(data).map(|(v, shortest, len)| (Some(v), shortest, len)),
        }
    }
}

macro_rules! impl_tuple_from_cbor {
    ($(($tuple_ty:ty, $map_expr:expr)),*) => {
        $(
            impl<T> FromCbor for $tuple_ty
            where
                T: FromCbor,
                T::Error: From<self::Error>,
            {
                type Error = T::Error;

                #[inline]
                fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
                    T::from_cbor(data).map(|(value, shortest, length)| ($map_expr(value, shortest, length), shortest, length))
                }
            }
        )*
    };
}

impl_tuple_from_cbor!(
    ((T, bool, usize), |value, shortest, length| (
        value, shortest, length
    )),
    ((T, bool), |value, shortest, _length| (value, shortest)),
    ((T, usize), |value, _shortest, length| (value, length))
);
