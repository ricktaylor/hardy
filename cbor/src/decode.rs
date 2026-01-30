/*!
A canonical CBOR decoder for parsing byte streams.

This module provides tools for decoding data from the Concise Binary Object
Representation (CBOR) format, as specified in
[RFC 8949](https://www.rfc-editor.org/rfc/rfc8949.html). The decoder is
designed to handle both simple and complex CBOR structures, including
definite and indefinite-length items, and semantic tags.

# Core Concepts

There are two primary ways to use the decoder:

1.  **Direct Deserialization with `FromCbor`:** For straightforward cases, you can
    implement the [`FromCbor`] trait for your types. This allows you to
    convert a CBOR byte slice directly into a Rust struct.

2.  **Streaming Parsing with `parse_*` functions:** For more complex or
    performance-sensitive scenarios, you can use the `parse_*` functions
    ([`parse_value`], [`parse_array`], [`parse_map`]) to process the CBOR
    stream piece by piece. This approach gives you fine-grained control and
    avoids intermediate allocations.

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
            let (x, sx) = a.parse()?;
            let (y, sy) = a.parse()?;
            Ok((Point { x, y }, shortest && sx && sy))
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
    UnsignedInteger(u64),
    NegativeInteger(u64),
    Bytes(Range<usize>),
    ByteStream(Vec<Range<usize>>),
    Text(&'b str),
    TextStream(&'a [&'b str]),
    Array(&'a mut Array<'b>),
    Map(&'a mut Map<'b>),
    False,
    True,
    Null,
    Undefined,
    Simple(u8),
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
    /// For simple types, this does nothing. For arrays and maps, it consumes all
    /// nested items until the end of the sequence is reached.
    pub fn skip(&mut self, mut max_recursion: usize) -> Result<bool, Error> {
        match self {
            Value::Array(a) => {
                if max_recursion == 0 {
                    return Err(Error::MaxRecursion);
                }
                max_recursion -= 1;
                a.skip_to_end(max_recursion).map(|s| s && a.is_definite())
            }
            Value::Map(m) => {
                if max_recursion == 0 {
                    return Err(Error::MaxRecursion);
                }
                max_recursion -= 1;
                m.skip_to_end(max_recursion).map(|s| s && m.is_definite())
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
                shortest = shortest && s;
                offset += o;
            }
            _ => break,
        }
    }
    Ok((tags, shortest, offset))
}

fn to_array<const N: usize>(data: &[u8]) -> Result<[u8; N], Error> {
    match data.len().cmp(&N) {
        core::cmp::Ordering::Less => Err(Error::NeedMoreData(N - data.len())),
        core::cmp::Ordering::Equal => Ok(data.try_into().unwrap()),
        core::cmp::Ordering::Greater => Ok(data[0..N].try_into().unwrap()),
    }
}

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

fn parse_data_minor(minor: u8, data: &[u8]) -> Result<(Range<usize>, bool, usize), Error> {
    let (data_len, shortest, len) = parse_uint_minor(minor, data)?;
    let data_len = data_len
        .checked_add(len as u64)
        .and_then(|data_len| (data_len <= usize::MAX as u64).then_some(data_len as usize))
        .ok_or(Error::TooBig)?;

    if data_len > data.len() {
        Err(Error::NeedMoreData(data_len - data.len()))
    } else {
        Ok((len..data_len, shortest, data_len))
    }
}

fn parse_data_chunked(major: u8, data: &[u8]) -> Result<(Vec<Range<usize>>, bool, usize), Error> {
    let mut chunks = Vec::new();
    let mut offset = 0;
    let mut shortest = true;
    while let Some(v) = data.get(offset) {
        offset += 1;

        if *v == 0xFF {
            return Ok((chunks, shortest, offset));
        }

        if v >> 5 != major {
            return Err(Error::InvalidChunk);
        }

        let (chunk, s, chunk_len) = parse_data_minor(v & 0x1F, &data[offset..])?;
        chunks.push(chunk.start + offset..chunk.end + offset);
        shortest = shortest && s;
        offset += chunk_len;
    }

    Err(Error::NeedMoreData(1))
}

/// Parses a single CBOR value from a byte slice and processes it with a closure.
///
/// This is the core low-level parsing function. It handles tags and determines
/// the major type of the next item in the slice, then passes a [`Value`]
/// representation to the provided closure `f`.
///
/// On success, it returns a tuple containing the result of the closure and the
/// total number of bytes consumed from the input slice.
pub fn parse_value<T, F, E>(data: &[u8], f: F) -> Result<(T, usize), E>
where
    F: FnOnce(Value, bool, &[u64]) -> Result<T, E>,
    E: From<Error>,
{
    let (tags, mut shortest, mut offset) = parse_tags(data)?;
    let Some(marker) = data.get(offset) else {
        return Err(Error::NeedMoreData(1).into());
    };
    offset += 1;

    match (marker >> 5, marker & 0x1F) {
        (0, minor) => {
            let (v, s, len) = parse_uint_minor(minor, &data[offset..])?;
            offset += len;
            f(Value::UnsignedInteger(v), shortest && s, &tags)
        }
        (1, minor) => {
            let (v, s, len) = parse_uint_minor(minor, &data[offset..])?;
            offset += len;
            f(Value::NegativeInteger(v), shortest && s, &tags)
        }
        (2, 31) => {
            /* Indefinite length byte string */
            let (mut v, s, len) = parse_data_chunked(2, &data[offset..])?;
            for t in v.iter_mut() {
                t.start += offset;
                t.end += offset;
            }
            offset += len;
            f(Value::ByteStream(v), shortest && s, &tags)
        }
        (2, minor) => {
            /* Known length byte string */
            let (t, s, len) = parse_data_minor(minor, &data[offset..])?;
            let t = t.start + offset..t.end + offset;
            offset += len;
            f(Value::Bytes(t), shortest && s, &tags)
        }
        (3, 31) => {
            /* Indefinite length text string */
            let data = &data[offset..];
            let (v, s, len) = parse_data_chunked(3, data)?;
            offset += len;
            let mut t = Vec::with_capacity(v.len());
            for b in v {
                t.push(core::str::from_utf8(&data[b]).map_err(Into::into)?);
            }
            f(Value::TextStream(&t), shortest && s, &tags)
        }
        (3, minor) => {
            /* Known length text string */
            let data = &data[offset..];
            let (t, s, len) = parse_data_minor(minor, data)?;
            offset += len;
            f(
                Value::Text(core::str::from_utf8(&data[t]).map_err(Into::into)?),
                shortest && s,
                &tags,
            )
        }
        (4, 31) => {
            /* Indefinite length array */
            let mut a = Array::new(data, None, &mut offset);
            let r = f(Value::Array(&mut a), shortest, &tags)?;
            a.complete(r).map_err(Into::into)
        }
        (4, minor) => {
            /* Known length array */
            let (count, s, len) = parse_uint_minor(minor, &data[offset..])?;
            offset += len;
            if count > usize::MAX as u64 {
                return Err(Error::TooBig.into());
            }
            let mut a = Array::new(data, Some(count as usize), &mut offset);
            let r = f(Value::Array(&mut a), shortest && s, &tags)?;
            a.complete(r).map_err(Into::into)
        }
        (5, 31) => {
            /* Indefinite length map */
            let mut m = Map::new(data, None, &mut offset);
            let r = f(Value::Map(&mut m), true, &tags)?;
            m.complete(r).map_err(Into::into)
        }
        (5, minor) => {
            /* Known length array */
            let (count, s, len) = parse_uint_minor(minor, &data[offset..])?;
            offset += len;
            if count > (usize::MAX as u64) / 2 {
                return Err(Error::TooBig.into());
            }
            let mut m = Map::new(data, Some((count * 2) as usize), &mut offset);
            let r = f(Value::Map(&mut m), shortest && s, &tags)?;
            m.complete(r).map_err(Into::into)
        }
        (6, _) => unreachable!(),
        (7, 20) => {
            /* False */
            f(Value::False, shortest, &tags)
        }
        (7, 21) => {
            /* True */
            f(Value::True, shortest, &tags)
        }
        (7, 22) => {
            /* Null */
            f(Value::Null, shortest, &tags)
        }
        (7, 23) => {
            /* Undefined */
            f(Value::Undefined, shortest, &tags)
        }
        (7, minor @ 0..=19) => {
            /* Unassigned simple type */
            f(Value::Simple(minor), shortest, &tags)
        }
        (7, 24) => {
            /* Unassigned simple type */
            let Some(v) = data.get(offset) else {
                return Err(Error::NeedMoreData(1).into());
            };
            offset += 1;
            if *v < 32 {
                return Err(Error::InvalidSimpleType(*v).into());
            }
            f(Value::Simple(*v), shortest, &tags)
        }
        (7, 25) => {
            /* FP16 */
            let v = half::f16::from_be_bytes(to_array(&data[offset..])?);
            offset += 2;
            f(Value::Float(v.into()), shortest, &tags)
        }
        (7, 26) => {
            /* FP32 */
            let v = f32::from_be_bytes(to_array(&data[offset..])?);
            offset += 4;
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
            f(Value::Float(v.into()), shortest, &tags)
        }
        (7, 27) => {
            /* FP64 */
            let v = f64::from_be_bytes(to_array(&data[offset..])?);
            offset += 8;
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
            f(Value::Float(v), shortest, &tags)
        }
        (7, minor) => {
            return Err(Error::InvalidSimpleType(minor).into());
        }
        _ => unreachable!(),
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
    parse_value(data, |value, shortest, tags| match value {
        Value::Array(a) => f(a, shortest, tags),
        _ => {
            Err(Error::IncorrectType("Array".to_string(), value.type_name(!tags.is_empty())).into())
        }
    })
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
    parse_value(data, |value, shortest, tags| match value {
        Value::Map(m) => f(m, shortest, tags),
        _ => Err(Error::IncorrectType("Map".to_string(), value.type_name(!tags.is_empty())).into()),
    })
}

/// A convenience function to decode a single value that implements [`FromCbor`].
///
/// This function is a shorthand for `T::from_cbor(data).map(|v| v.0)`. It
/// decodes the value and discards the `shortest` and `len` information,
/// returning only the decoded object.
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

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        parse_value(data, |value, shortest, tags| match value {
            Value::UnsignedInteger(n) => Ok((n, shortest && tags.is_empty())),
            value => Err(Error::IncorrectType(
                "Untagged Unsigned Integer".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

macro_rules! impl_int_from_cbor {
    ($($ty:ty),*) => {
        $(
            impl FromCbor for $ty {
                type Error = self::Error;

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

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        parse_value(data, |value, shortest, tags| match value {
            Value::UnsignedInteger(n) => Ok((i64::try_from(n)?, shortest && tags.is_empty())),
            Value::NegativeInteger(n) => {
                Ok((-1i64 - i64::try_from(n)?, shortest && tags.is_empty()))
            }
            value => Err(Error::IncorrectType(
                "Untagged Integer".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

macro_rules! impl_float_from_cbor {
    ($(($ty:ty, $convert_expr:expr)),*) => {
        $(
            impl FromCbor for $ty {
                type Error = self::Error;

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

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        parse_value(data, |value, shortest, tags| match value {
            Value::Float(f) => Ok((f, shortest && tags.is_empty())),
            value => Err(Error::IncorrectType(
                "Untagged Float".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

impl FromCbor for bool {
    type Error = self::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        parse_value(data, |value, shortest, tags| match value {
            Value::False => Ok((false, shortest && tags.is_empty())),
            Value::True => Ok((true, shortest && tags.is_empty())),
            value => Err(Error::IncorrectType(
                "Untagged Boolean".to_string(),
                value.type_name(!tags.is_empty()),
            )),
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

impl<T> FromCbor for Option<T>
where
    T: FromCbor,
    T::Error: From<self::Error>,
{
    type Error = T::Error;

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
