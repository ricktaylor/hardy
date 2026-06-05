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

3.  **Low-level peek with [`Head`]:** For optimised parsing where
    you want to dispatch on type and length without using a closure or
    materialising indefinite-length contents, parse a [`Head`] and
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

## 3. Low-level Head

Parse a [`Head`] when you want to dispatch on type and length without
constructing a [`Value`] or running a closure. Useful on hot paths where
you intend to stream a byte/text payload straight to a sink, or to skip
an item you know you don't need.

```
use hardy_cbor::decode::{self, FromCbor, Head, Marker};

// CBOR for `24(h'deadbeef')` — tag 24 wrapping a 4-byte byte string
let bytes = &[0xd8, 0x18, 0x44, 0xde, 0xad, 0xbe, 0xef];

// Peek at just the head: get the type, tags, and length of the head
// itself without touching the payload.
let (head, _shortest, head_len) = Head::from_cbor(bytes).unwrap();
assert_eq!(head.tags.as_slice(), &[24]);

match head.marker {
    Marker::Bytes(Some(payload_len)) => {
        // The payload sits at `head_len..head_len + payload_len`.
        // Stream it directly — no intermediate allocation or Value.
        let payload = &bytes[head_len..head_len + payload_len as usize];
        assert_eq!(payload, b"\xde\xad\xbe\xef");
    }
    _ => panic!("expected a definite-length byte string"),
}
```

[RFC 8949]: https://www.rfc-editor.org/rfc/rfc8949.html
*/
use super::*;
use core::{ops::Range, str::Utf8Error};
use num_traits::{FromPrimitive, ToPrimitive};
use thiserror::Error;

mod head;
mod impls;
mod series;

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
    ///
    /// Decoding reads a single item from the **front** of `data` and stops; the
    /// slice is not required to contain exactly one item, and the returned length
    /// is how many bytes that item occupied. Anything after it is left untouched.
    /// This is what makes decoders composable — an enclosing array, map or
    /// sequence decodes a field, then advances by the returned length to the next.
    ///
    /// A consequence is that `from_cbor` is **not**, on its own, a guard against
    /// trailing data: only implementations that consume to the end of the slice
    /// (those built on [`parse_sequence`]) reject it; implementations built on a
    /// single value, array or map silently ignore any bytes past their item. When
    /// `data` is a standalone slice that must hold exactly one item, compare the
    /// returned length against `data.len()` yourself, or use [`parse_exact`].
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error>;
}

/// A type alias for a generic, untyped CBOR sequence.
pub type Sequence<'a> = series::Series<'a, 0>;
/// A type alias for a [`Series`] that represents a CBOR array.
pub type Array<'a> = series::Series<'a, 1>;
/// A type alias for a [`Series`] that represents a CBOR map.
pub type Map<'a> = series::Series<'a, 2>;
/// The head of a single CBOR data item.
pub use head::{Head, Marker};
/// A stateful iterator for decoding a sequence of CBOR items (e.g., an array or map).
pub use series::Series;

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
        let prefix = if tagged { "Tagged" } else { "Untagged" };
        match self {
            Value::UnsignedInteger(_) => format!("{prefix} Unsigned Integer"),
            Value::NegativeInteger(_) => format!("{prefix} Negative Integer"),
            Value::Bytes(_) => format!("{prefix} Definite-length Byte String"),
            Value::ByteStream(_) => format!("{prefix} Indefinite-length Byte String"),
            Value::Text(_) => format!("{prefix} Definite-length Text String"),
            Value::TextStream(_) => format!("{prefix} Indefinite-length Text String"),
            Value::Array(a) if a.is_definite() => format!("{prefix} Definite-length Array"),
            Value::Array(_) => format!("{prefix} Indefinite-length Array"),
            Value::Map(m) if m.is_definite() => format!("{prefix} Definite-length Map"),
            Value::Map(_) => format!("{prefix} Indefinite-length Map"),
            Value::False => format!("{prefix} False"),
            Value::True => format!("{prefix} True"),
            Value::Null => format!("{prefix} Null"),
            Value::Undefined => format!("{prefix} Undefined"),
            Value::Simple(v) => format!("{prefix} Simple Value {v}"),
            Value::Float(_) => format!("{prefix} Float"),
        }
    }

    /// Finishes consuming this value, advancing the cursor past any nested
    /// items.
    ///
    /// For scalars this is a no-op. For arrays and maps it walks every
    /// remaining nested item until the end of the sequence. Returns
    /// whether every nested marker was minimally encoded;
    /// definite-vs-indefinite of the sequence itself is not considered
    /// (use [`Series::is_definite`] on the value for that).
    ///
    /// # Which skip to use
    ///
    /// - If you do **not** need the [`Value`], call [`decode::skip_value`]
    ///   on the byte slice. It walks the wire format directly and pays no
    ///   chunk-list or `Series` allocation.
    /// - If you are inside a sequence and want to skip a single item
    ///   without parsing it, call [`Series::skip_value`].
    /// - If you have already called [`decode::parse_value`] (typically
    ///   because you needed the `Value` to inspect or format it) and now
    ///   need to advance the cursor past the rest of that value, use this
    ///   method. The allocations have already been paid; this just
    ///   finishes the parse.
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

/// Skips over the next CBOR data item without materialising its contents.
///
/// Walks the encoding using [`Head`] dispatch. Indefinite-length
/// string chunks are walked in place with no allocation; arrays and maps
/// are drained through a stack-only [`Series`] cursor — no `Vec` for
/// chunk lists or nested values is constructed.
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
    let (marker, mut shortest, mut offset) = parse::<(Head, bool, usize)>(data)?;
    match marker.marker {
        Marker::Bytes(Some(v)) | Marker::Text(Some(v)) => {
            Ok((shortest, offset_extent(data, offset, v)?.end))
        }
        Marker::Bytes(None) => loop {
            // Check for the break stop code directly, before parsing a
            // Head — Head::from_cbor rejects 0xFF as it is not a data
            // item. This mirrors `Series::at_end`.
            if data.get(offset) == Some(&0xFF) {
                return Ok((shortest, offset + 1));
            }
            let (inner_marker, inner_shortest, len) =
                parse::<(Head, bool, usize)>(&data[offset..])?;
            offset = offset.checked_add(len).ok_or(Error::TooBig)?;

            match inner_marker.marker {
                Marker::Bytes(Some(len)) if inner_marker.tags.is_empty() => {
                    offset = offset_extent(data, offset, len)?.end;
                    shortest &= inner_shortest;
                }
                _ => return Err(Error::InvalidChunk),
            }
        },
        Marker::Text(None) => loop {
            if data.get(offset) == Some(&0xFF) {
                return Ok((shortest, offset + 1));
            }
            let (inner_marker, inner_shortest, len) =
                parse::<(Head, bool, usize)>(&data[offset..])?;
            offset = offset.checked_add(len).ok_or(Error::TooBig)?;

            match inner_marker.marker {
                Marker::Text(Some(len)) if inner_marker.tags.is_empty() => {
                    offset = offset_extent(data, offset, len)?.end;
                    shortest &= inner_shortest;
                }
                _ => return Err(Error::InvalidChunk),
            }
        },
        Marker::Array(count) => {
            if max_recursion == 0 {
                return Err(Error::MaxRecursion);
            }
            max_recursion -= 1;
            Array::try_new(data, count, &mut offset)?
                .skip_to_end(max_recursion)
                .map(|s| (shortest & s, offset))
        }
        Marker::Map(count) => {
            if max_recursion == 0 {
                return Err(Error::MaxRecursion);
            }
            max_recursion -= 1;
            Map::try_new(data, count, &mut offset)?
                .skip_to_end(max_recursion)
                .map(|s| (shortest & s, offset))
        }
        _ => Ok((shortest, offset)),
    }
}

#[inline]
fn offset_extent(data: &[u8], offset: usize, len: u64) -> Result<Range<usize>, Error> {
    let end = usize::try_from(len)
        .ok()
        .and_then(|len| offset.checked_add(len))
        .ok_or(Error::TooBig)?;
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
/// For finer control without a closure, parse a [`Head`] instead.
///
/// On success, returns the result of `f` and the total number of bytes
/// consumed from the input slice.
pub fn parse_value<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(Value, bool, &[u64]) -> Result<T, E>,
    E: From<Error>,
{
    let (marker, mut shortest, mut offset) = parse::<(Head, bool, usize)>(data)?;
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
                // Break stop code terminates the indefinite-length string.
                // Detected directly because Head::from_cbor rejects 0xFF.
                if data.get(offset) == Some(&0xFF) {
                    offset += 1;
                    break f(Value::ByteStream(chunks), shortest, &marker.tags);
                }
                let (inner_marker, inner_shortest, len) =
                    parse::<(Head, bool, usize)>(&data[offset..])?;

                offset = offset.checked_add(len).ok_or(Error::TooBig)?;

                match inner_marker.marker {
                    Marker::Bytes(Some(len)) if inner_marker.tags.is_empty() => {
                        let extent = offset_extent(data, offset, len)?;
                        offset = extent.end;
                        chunks.push(extent);
                        shortest &= inner_shortest;
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
                if data.get(offset) == Some(&0xFF) {
                    offset += 1;
                    break f(Value::TextStream(&chunks), shortest, &marker.tags);
                }
                let (inner_marker, inner_shortest, len) =
                    parse::<(Head, bool, usize)>(&data[offset..])?;

                offset = offset.checked_add(len).ok_or(Error::TooBig)?;

                match inner_marker.marker {
                    Marker::Text(Some(len)) if inner_marker.tags.is_empty() => {
                        let extent = offset_extent(data, offset, len)?;
                        offset = extent.end;
                        chunks.push(core::str::from_utf8(&data[extent]).map_err(Into::into)?);
                        shortest &= inner_shortest;
                    }
                    _ => {
                        return Err(Error::InvalidChunk.into());
                    }
                }
            }
        }
        Marker::Array(count) => {
            let mut a = Array::try_new(data, count, &mut offset)?;
            let r = f(Value::Array(&mut a), shortest, &marker.tags)?;
            a.complete(r).map_err(Into::into)
        }
        Marker::Map(count) => {
            let mut m = Map::try_new(data, count, &mut offset)?;
            let r = f(Value::Map(&mut m), shortest, &marker.tags)?;
            m.complete(r).map_err(Into::into)
        }
        Marker::False => f(Value::False, shortest, &marker.tags),
        Marker::True => f(Value::True, shortest, &marker.tags),
        Marker::Null => f(Value::Null, shortest, &marker.tags),
        Marker::Undefined => f(Value::Undefined, shortest, &marker.tags),
        Marker::Simple(v) => f(Value::Simple(v), shortest, &marker.tags),
        Marker::Float(v) => f(Value::Float(v), shortest, &marker.tags),
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
    let (marker, shortest, mut offset) = parse::<(Head, bool, usize)>(data)?;
    match marker.marker {
        Marker::Array(count) => {
            let mut a = Array::try_new(data, count, &mut offset)?;
            let r = f(&mut a, shortest, &marker.tags)?;
            a.complete(r).map(|r| (r, offset)).map_err(Into::into)
        }
        _ => Err(Error::IncorrectType("Array".to_string(), marker.to_string()).into()),
    }
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
    let (marker, shortest, mut offset) = parse::<(Head, bool, usize)>(data)?;
    match marker.marker {
        Marker::Map(count) => {
            let mut m = Map::try_new(data, count, &mut offset)?;
            let r = f(&mut m, shortest, &marker.tags)?;
            m.complete(r).map(|r| (r, offset)).map_err(Into::into)
        }
        _ => Err(Error::IncorrectType("Map".to_string(), marker.to_string()).into()),
    }
}

/// A convenience function to decode a single value that implements [`FromCbor`].
///
/// This function is a shorthand for `T::from_cbor(data).map(|v| v.0)`. It
/// decodes the value and discards the `shortest` and `len` information,
/// returning only the decoded object.
///
/// Because `len` is discarded, any bytes after the first item are ignored. Use
/// [`parse_exact`] instead when `data` must hold exactly one item, so that
/// trailing bytes can't be smuggled past the decoder.
#[inline]
pub fn parse<T>(data: &[u8]) -> Result<T, T::Error>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    T::from_cbor(data).map(|v| v.0)
}

/// Decode a single value of type `T`, requiring it to consume the **whole**
/// slice: any trailing bytes after the item are rejected as
/// [`Error::AdditionalItems`].
///
/// Use this — rather than [`parse`], which ignores trailing data — when `data`
/// must hold exactly one item (e.g. the body of a bundle block), so that extra
/// bytes can't be smuggled past the decoder.
#[inline]
pub fn parse_exact<T>(data: &[u8]) -> Result<T, T::Error>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    let (value, _, len) = T::from_cbor(data)?;
    if len == data.len() {
        Ok(value)
    } else {
        Err(self::Error::AdditionalItems.into())
    }
}
