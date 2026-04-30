/*!
A canonical CBOR encoder designed for performance and flexibility.

This module provides tools for encoding Rust data structures into the
Concise Binary Object Representation (CBOR) format, as specified in
[RFC 8949]. The encoder
prioritizes canonical output, ensuring that a given data structure always
produces the same, shortest possible byte representation.

# Core Concepts

The two primary components of this library are the [`ToCbor`] trait and the
[`Encoder`] struct.

- **[`ToCbor`] trait:** Implement this trait for your types to make them
  directly encodable. The library provides implementations for most Rust
  primitive types, strings, slices, and tuples.

- **[`Encoder`] struct:** A stateful encoder that builds the CBOR byte
  stream. It can be used for more complex, procedural encoding scenarios,
  such as building indefinite-length arrays or maps.

# Usage

## 1. Manual `ToCbor` Implementation

To make a custom type encodable, implement the [`ToCbor`] trait.

```
use hardy_cbor::buffer::encoder::{self, BufferEncoder, ToCbor};

struct Point {
    x: i32,
    y: i32,
}

impl ToCbor for Point {
    type Result = ();

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.emit_array(Some(2), |a| {
            a.emit(&self.x);
            a.emit(&self.y);
        });
    }
}

let point = Point { x: 10, y: -20 };
let (bytes, _) = encoder::emit(&point);
assert_eq!(bytes, &[0x82, 0x0A, 0x33]);
```

## 2. Using Helper Structs

The library provides helper structs like [`Tagged`], [`Bytes`], and [`Raw`] to
control the output format.

```
use hardy_cbor::buffer::encoder::{self, Tagged, Bytes};

let data = b"hello";
let tagged_data = Tagged::<24, _>(&Bytes(data));
let (bytes, _) = encoder::emit(&tagged_data);
assert_eq!(bytes, &[0xd8, 0x18, 0x45, 0x68, 0x65, 0x6c, 0x6c, 0x6f]);
```

[RFC 8949]: https://www.rfc-editor.org/rfc/rfc8949.html
*/
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

/// A trait for types that can be encoded into CBOR format.
///
/// This trait is the foundation of the encoding system. By implementing `ToCbor`
/// for a type, you define how it should be represented as CBOR. The library
/// provides implementations for most primitive types, `&str`, `String`, slices,
/// and tuples up to 16 elements.
pub trait ToCbor {
    /// The result type returned by the encoding operation.
    ///
    /// For most types, this is `()`. For types that wrap a slice or other
    /// borrowed data (like [`Bytes`] or [`Raw`]), this is typically a `Range<usize>`
    /// indicating the position of the encoded data within the final byte buffer.
    type Result;

    /// Encodes the value into the given [`Encoder`].
    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result;
}

/// A stateful, streaming encoder for building a CBOR byte stream.
///
/// The `Encoder` is used to procedurally construct a CBOR object. It manages
/// a byte buffer and provides methods to emit various CBOR data types.
pub struct BufferEncoder {
    data: Vec<u8>,
}

impl Default for BufferEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferEncoder {
    /// Creates a new, empty `Encoder`.
    #[inline]
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }

    /// Consumes the encoder and returns the generated CBOR byte vector.
    #[inline]
    pub fn build(self) -> Vec<u8> {
        self.data
    }

    /// Returns the current length of the encoded data in bytes.
    #[inline]
    pub fn offset(&self) -> usize {
        self.data.len()
    }

    fn emit_extend(&mut self, b: &[u8]) -> Range<usize> {
        let start = self.offset();
        self.data.extend_from_slice(b);
        start..self.offset()
    }

    fn emit_uint_minor(&mut self, major: u8, val: u64) {
        let mut head = [0u8; 9];
        let n = crate::head::encode_head(&mut head, major, val);
        self.data.extend_from_slice(&head[..n]);
    }

    fn emit_tag(&mut self, tag: u64) -> &mut Self {
        self.emit_uint_minor(6, tag);
        self
    }

    /// Encodes a value that implements the [`ToCbor`] trait.
    ///
    /// This is the primary method for writing data to the encoder.
    #[inline]
    pub fn emit<T>(&mut self, value: &T) -> T::Result
    where
        T: ToCbor + ?Sized,
    {
        value.to_cbor(self)
    }

    fn emit_raw<V>(&mut self, data: &V) -> Range<usize>
    where
        V: AsRef<[u8]> + ?Sized,
    {
        let start = self.offset();
        self.data.extend_from_slice(data.as_ref());
        start..self.offset()
    }

    fn emit_bytes<V>(&mut self, value: &V) -> Range<usize>
    where
        V: AsRef<[u8]> + ?Sized,
    {
        let value = value.as_ref();
        self.emit_uint_minor(2, value.len() as u64);
        self.emit_extend(value)
    }

    fn emit_string<V>(&mut self, value: &V) -> Range<usize>
    where
        V: AsRef<str> + ?Sized,
    {
        let value = value.as_ref().as_bytes();
        self.emit_uint_minor(3, value.len() as u64);
        self.emit_extend(value)
    }

    /// Emits an indefinite-length byte stream.
    ///
    /// The provided closure receives a [`ByteStream`] helper, which can be used
    /// to emit a sequence of definite-length byte string chunks.
    ///
    /// ```
    /// use hardy_cbor::buffer::encoder;
    /// let bytes = encoder::emit_byte_stream(|s| {
    ///     s.emit(&[1u8, 2]);
    ///     s.emit(&[3u8, 4, 5]);
    /// });
    /// assert_eq!(bytes, &[0x5f, 0x42, 0x01, 0x02, 0x43, 0x03, 0x04, 0x05, 0xff]);
    /// ```
    pub fn emit_byte_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut ByteStream),
    {
        let mut s = ByteStream::new(self);
        f(&mut s);
        s.end()
    }

    /// Emits an indefinite-length text stream.
    ///
    /// The provided closure receives a [`TextStream`] helper to emit string chunks.
    pub fn emit_text_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut TextStream),
    {
        let mut s = TextStream::new(self);
        f(&mut s);
        s.end()
    }

    /// Emits a CBOR array.
    ///
    /// If `count` is `Some`, a definite-length array is created.
    /// If `count` is `None`, an indefinite-length array is created.
    /// The closure receives an [`Array`] helper to emit the array's elements.
    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Array),
    {
        let mut a = Array::new(self, count);
        f(&mut a);
        a.end();
    }

    /// Emits a CBOR array.
    ///
    /// If `count` is `Some`, a definite-length array is created.
    /// If `count` is `None`, an indefinite-length array is created.
    /// The closure receives an [`Array`] helper to emit the array's elements.
    pub fn try_emit_array<F, E>(&mut self, count: Option<usize>, f: F) -> Result<(), E>
    where
        F: FnOnce(&mut Array) -> Result<(), E>,
    {
        let mut a = Array::new(self, count);
        f(&mut a)?;
        a.end();
        Ok(())
    }

    fn emit_array_slice<V, T>(&mut self, values: &V)
    where
        V: AsRef<[T]> + ?Sized,
        T: ToCbor + Sized,
    {
        let values = values.as_ref();
        let mut a = Array::new(self, Some(values.len()));
        for value in values {
            a.emit(value);
        }
        a.end()
    }

    /// Emits a CBOR map.
    ///
    /// If `count` is `Some`, a definite-length map with that many key-value pairs is created.
    /// If `count` is `None`, an indefinite-length map is created.
    /// The closure receives a [`Map`] helper to emit the map's entries.
    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Map),
    {
        let mut m = Map::new(self, count);
        f(&mut m);
        m.end();
    }

    /// Emits a CBOR map.
    ///
    /// If `count` is `Some`, a definite-length map with that many key-value pairs is created.
    /// If `count` is `None`, an indefinite-length map is created.
    /// The closure receives a [`Map`] helper to emit the map's entries.
    pub fn try_emit_map<F, E>(&mut self, count: Option<usize>, f: F) -> Result<(), E>
    where
        F: FnOnce(&mut Map) -> Result<(), E>,
    {
        let mut m = Map::new(self, count);
        f(&mut m)?;
        m.end();
        Ok(())
    }
}

/// A wrapper to encode a value with a CBOR tag.
///
/// Tags provide additional semantic information about the encoded data.
/// These can be nested to add multiple tags.
pub struct Tagged<'a, const TAG: u64, T>(pub &'a T)
where
    T: ToCbor + ?Sized;

impl<'a, const TAG: u64, T> ToCbor for Tagged<'a, TAG, T>
where
    T: ToCbor + ?Sized,
{
    type Result = T::Result;

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.emit_tag(TAG).emit(self.0)
    }
}

/// A wrapper for emitting CBOR tags with runtime-determined tag numbers.
///
/// Unlike [`Tagged<const TAG, T>`], which requires the tag number to be known at compile time,
/// `RuntimeTagged` allows you to specify the tag number at runtime. This is useful when parsing
/// or generating CBOR data where tag numbers are determined dynamically.
///
/// # Examples
///
/// ```
/// use hardy_cbor::buffer::encoder::{self, RuntimeTagged};
///
/// let data = b"hello";
/// let tagged = RuntimeTagged(24u64, &data);
/// let bytes = encoder::emit(&tagged).0;
/// ```
///
/// # CBOR Encoding
///
/// CBOR tags use major type 6, with the tag number encoded using the same variable-length
/// encoding as unsigned integers. The tagged value immediately follows the tag header.
pub struct RuntimeTagged<'a, T>(pub u64, pub &'a T)
where
    T: ToCbor + ?Sized;

impl<'a, T> ToCbor for RuntimeTagged<'a, T>
where
    T: ToCbor + ?Sized,
{
    type Result = T::Result;

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.emit_tag(self.0);
        encoder.emit(self.1)
    }
}

/// A wrapper to write raw bytes directly into the stream without any CBOR encoding.
///
/// This is useful for embedding pre-encoded CBOR data or other byte-oriented
/// formats within a CBOR stream.
pub struct Raw<'a, V>(pub &'a V)
where
    V: AsRef<[u8]> + ?Sized;

impl<'a, V> ToCbor for Raw<'a, V>
where
    V: AsRef<[u8]> + ?Sized,
{
    type Result = Range<usize>;

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.emit_raw(self.0)
    }
}

/// A wrapper to write the CBOR discriminator bytes of a byte string directly into the stream
///
/// This is useful for embedding pre-encoded CBOR data or other byte-oriented
/// formats within a CBOR stream.  Use with `Raw`.
pub struct BytesHeader<'a, V>(pub &'a V)
where
    V: AsRef<[u8]> + ?Sized;

impl<'a, V> ToCbor for BytesHeader<'a, V>
where
    V: AsRef<[u8]> + ?Sized,
{
    type Result = ();

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.emit_uint_minor(2, self.0.as_ref().len() as u64);
    }
}

/// A wrapper to encode a byte slice as a definite-length CBOR byte string.
///
/// By default, a `&[u8]` is encoded as a CBOR array of integers. Use this
/// wrapper to encode it as a byte string instead.
pub struct Bytes<'a, V>(pub &'a V)
where
    V: AsRef<[u8]> + ?Sized;

impl<'a, V> ToCbor for Bytes<'a, V>
where
    V: AsRef<[u8]> + ?Sized,
{
    type Result = Range<usize>;

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.emit_bytes(self.0)
    }
}

/// A helper for building an indefinite-length CBOR byte stream.
pub struct ByteStream<'a> {
    encoder: &'a mut BufferEncoder,
}

impl<'a> ByteStream<'a> {
    fn new(encoder: &'a mut BufferEncoder) -> Self {
        encoder.data.push((2 << 5) | 31);
        Self { encoder }
    }

    /// Emits a single, definite-length chunk of bytes into the stream.
    pub fn emit<V>(&mut self, value: &V)
    where
        V: AsRef<[u8]> + ?Sized,
    {
        self.encoder.emit_bytes(value);
    }

    fn end(self) {
        self.encoder.data.push(0xFF)
    }
}

/// A helper for building an indefinite-length CBOR text stream.
pub struct TextStream<'a> {
    encoder: &'a mut BufferEncoder,
}

impl<'a> TextStream<'a> {
    fn new(encoder: &'a mut BufferEncoder) -> Self {
        encoder.data.push((3 << 5) | 31);
        Self { encoder }
    }

    /// Emits a single, definite-length chunk of text into the stream.
    pub fn emit<V>(&mut self, value: &V)
    where
        V: AsRef<str> + ?Sized,
    {
        self.encoder.emit_string(value);
    }

    fn end(self) {
        self.encoder.data.push(0xFF)
    }
}

/// A helper for building a CBOR sequence (an array or a map).
///
/// This struct is created by [`Encoder::emit_array`] or [`Encoder::emit_map`].
/// It provides methods to emit elements into the sequence.
pub struct Sequence<'a, const D: usize> {
    encoder: &'a mut BufferEncoder,
    start: usize,
    count: Option<usize>,
    idx: usize,
}

/// A type alias for a [`Sequence`] that represents a CBOR array.
pub type Array<'a> = Sequence<'a, 1>;
/// A type alias for a [`Sequence`] that represents a CBOR map.
pub type Map<'a> = Sequence<'a, 2>;

impl<'a, const D: usize> Sequence<'a, D> {
    fn new(encoder: &'a mut BufferEncoder, count: Option<usize>) -> Self {
        let start = encoder.offset();
        if let Some(count) = count {
            encoder.emit_uint_minor(if D == 1 { 4 } else { 5 }, count as u64);
        } else {
            encoder.data.push((if D == 1 { 4 } else { 5 } << 5) | 31);
        }
        Self {
            start,
            encoder,
            count: count.map(|c| c * D),
            idx: 0,
        }
    }

    /// Returns the number of bytes written for this sequence so far.
    #[inline]
    pub fn offset(&self) -> usize {
        self.encoder.offset() - self.start
    }

    fn next_field(&mut self) -> &mut BufferEncoder {
        self.idx += 1;
        match self.count {
            Some(count) if self.idx > count => {
                panic!("Too many items added to definite length sequence")
            }
            _ => {}
        };
        self.encoder
    }

    fn end(self) {
        let Some(count) = self.count else {
            return self.encoder.data.push(0xFF);
        };
        if self.idx != count {
            panic!(
                "Definite length sequence is short of items: {}, expected {count}",
                self.idx
            );
        }
    }

    /// Skips emitting a value.
    #[inline]
    pub fn skip_value(&mut self) {
        self.next_field();
    }

    /// Emits a value into the sequence.
    #[inline]
    pub fn emit<T>(&mut self, value: &T) -> T::Result
    where
        T: ToCbor + ?Sized,
    {
        self.next_field().emit(value)
    }

    /// Emits an indefinite-length byte stream into the sequence.
    pub fn emit_byte_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut ByteStream),
    {
        self.next_field().emit_byte_stream(f)
    }

    /// Emits an indefinite-length text stream into the sequence.
    pub fn emit_text_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut TextStream),
    {
        self.next_field().emit_text_stream(f)
    }

    /// Emits a nested array into the sequence.
    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Array),
    {
        self.next_field().emit_array(count, f)
    }

    /// Emits a nested map into the sequence.
    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Map),
    {
        self.next_field().emit_map(count, f)
    }

    /// Emits a nested array into the sequence.
    pub fn try_emit_array<F, E>(&mut self, count: Option<usize>, f: F) -> Result<(), E>
    where
        F: FnOnce(&mut Array) -> Result<(), E>,
    {
        self.next_field().try_emit_array(count, f)
    }

    /// Emits a nested map into the sequence.
    pub fn try_emit_map<F, E>(&mut self, count: Option<usize>, f: F) -> Result<(), E>
    where
        F: FnOnce(&mut Map) -> Result<(), E>,
    {
        self.next_field().try_emit_map(count, f)
    }
}

/// Blanket implementation for references, allowing `&T` to be encoded where `T` is encodable.
impl<T> ToCbor for &T
where
    T: ToCbor,
{
    type Result = T::Result;

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        (*self).to_cbor(encoder)
    }
}

macro_rules! impl_uint_to_cbor {
    ($($ty:ty),*) => {
        $(
            impl ToCbor for $ty {
                type Result = ();

                #[inline]
                fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
                    encoder.emit_uint_minor(0, *self as u64)
                }
            }
        )*
    };
}

impl_uint_to_cbor!(u8, u16, u32, u64, usize);

macro_rules! impl_int_to_cbor {
    ($($ty:ty),*) => {
        $(
            impl ToCbor for $ty {
                type Result = ();

                #[inline]
                fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
                    if *self >= 0 {
                        encoder.emit_uint_minor(0, *self as u64)
                    } else {
                        encoder.emit_uint_minor(1, self.unsigned_abs() as u64 - 1)
                    }
                }
            }
        )*
    };
}

impl_int_to_cbor!(i8, i16, i32, i64, isize);

impl ToCbor for f64 {
    type Result = ();

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        let f16_val = half::f16::from_f64(*self);
        if f16_val.to_f64() == *self {
            encoder.data.push((7 << 5) | 25);
            encoder.data.extend_from_slice(&f16_val.to_be_bytes());
        } else {
            let f32_val = *self as f32;
            if f32_val as f64 == *self {
                encoder.data.push((7 << 5) | 26);
                encoder.data.extend_from_slice(&f32_val.to_be_bytes());
            } else {
                encoder.data.push((7 << 5) | 27);
                encoder.data.extend_from_slice(&self.to_be_bytes());
            }
        }
    }
}

impl ToCbor for f32 {
    type Result = ();

    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        let f16_val = half::f16::from_f32(*self);
        if f16_val.to_f32() == *self {
            encoder.data.push((7 << 5) | 25);
            encoder.data.extend_from_slice(&f16_val.to_be_bytes());
        } else {
            encoder.data.push((7 << 5) | 26);
            encoder.data.extend_from_slice(&self.to_be_bytes());
        }
    }
}

impl ToCbor for half::f16 {
    type Result = ();

    #[inline]
    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.data.push((7 << 5) | 25);
        encoder.data.extend(self.to_be_bytes())
    }
}

impl ToCbor for bool {
    type Result = ();

    #[inline]
    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.data.push((7 << 5) | if *self { 21 } else { 20 })
    }
}

macro_rules! impl_string_to_cbor {
    ($( $value_type:ty),*) => {
        $(
            impl ToCbor for $value_type {
                type Result = Range<usize>;

                #[inline]
                fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
                    encoder.emit_string(self)
                }
            }
        )*
    };
}

impl_string_to_cbor!(str, String);

impl<T> ToCbor for [T]
where
    T: ToCbor,
{
    type Result = ();

    #[inline]
    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.emit_array_slice(self)
    }
}

impl<T, const N: usize> ToCbor for [T; N]
where
    T: ToCbor,
{
    type Result = ();

    #[inline]
    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        encoder.emit_array_slice(self)
    }
}

impl<T> ToCbor for Option<T>
where
    T: ToCbor,
{
    type Result = Option<T::Result>;
    fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
        match self {
            Some(value) => Some(encoder.emit(value)),
            None => {
                encoder.data.push((7 << 5) | 23);
                None
            }
        }
    }
}

/// A convenience function to encode a single value into a `Vec<u8>`.
///
/// This creates an [`Encoder`], encodes the value, and returns the resulting
/// bytes along with the `ToCbor::Result`.
pub fn emit<T>(value: &T) -> (Vec<u8>, T::Result)
where
    T: ToCbor + ?Sized,
{
    let mut e = BufferEncoder::new();
    let r = e.emit(value);
    (e.build(), r)
}

macro_rules! impl_stream_emit_functions {
    ($(( $method:ident,  $stream_type:ty)),*) => {
        $(
            #[doc = concat!("A convenience function to encode a single ", stringify!($stream_type), " into a `Vec<u8>`.")]
            pub fn $method<F>(f: F) -> Vec<u8>
            where
                F: FnOnce(&mut $stream_type),
            {
                let mut e = BufferEncoder::new();
                e.$method(f);
                e.build()
            }
        )*
    };
}

impl_stream_emit_functions!(
    (emit_byte_stream, ByteStream),
    (emit_text_stream, TextStream)
);

macro_rules! impl_collection_emit_functions {
    ($(( $method:ident, $try_method:ident,$collection_type:ty)),*) => {
        $(
            #[doc = concat!("A convenience function to encode a single ", stringify!($collection_type), " into a `Vec<u8>`.")]
            pub fn $method<F>(count: Option<usize>, f: F) -> Vec<u8>
            where
                F: FnOnce(&mut $collection_type),
            {
                let mut e = BufferEncoder::new();
                e.$method(count, f);
                e.build()
            }

            #[doc = concat!("A convenience function to encode a single ", stringify!($collection_type), " into a `Vec<u8>` with a Result type.")]
            pub fn $try_method<F,E>(count: Option<usize>, f: F) -> Result<Vec<u8>,E>
            where
                F: FnOnce(&mut $collection_type) -> Result<(),E>,
            {
                let mut e = BufferEncoder::new();
                e.$try_method(count, f)?;
                Ok(e.build())
            }
        )*
    };
}

impl_collection_emit_functions!(
    (emit_array, try_emit_array, Array),
    (emit_map, try_emit_map, Map)
);

macro_rules! impl_tuple_emit_functions {
    // The first argument `$len:expr` captures the tuple's length.
    // The `( $($name:ident, $index:tt),* )` part matches a comma-separated list of
    // pairs, like `(T1, 0), (T2, 1)`.
    // `$name` will be the generic type identifier (e.g., T1, T2).
    // `$index` will be the numeric tuple index (e.g., 0, 1).
    ( $len:expr; $( ($name:ident, $index:tt) ),* ) => {
        impl<$($name: ToCbor),*> ToCbor for ($($name,)*) {
            type Result = ();
            fn to_cbor(&self, encoder: &mut BufferEncoder) -> Self::Result {
                encoder.emit_array(Some($len),|a| {
                    $( a.emit(&self.$index); )*
                })
            }
        }
    };
}

// Now, we call the macro to generate the implementations for tuples
// containing 2 to 16 elements, passing the length each time.
impl_tuple_emit_functions!(2; (T0, 0), (T1, 1));
impl_tuple_emit_functions!(3; (T0, 0), (T1, 1), (T2, 2));
impl_tuple_emit_functions!(4; (T0, 0), (T1, 1), (T2, 2), (T3, 3));
impl_tuple_emit_functions!(5; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4));
impl_tuple_emit_functions!(6; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5));
impl_tuple_emit_functions!(7; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6));
impl_tuple_emit_functions!(8; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7));
impl_tuple_emit_functions!(9; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7), (T8, 8));
impl_tuple_emit_functions!(10; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7), (T8, 8), (T9, 9));
impl_tuple_emit_functions!(11; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7), (T8, 8), (T9, 9), (T10, 10));
impl_tuple_emit_functions!(12; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7), (T8, 8), (T9, 9), (T10, 10), (T11, 11));
impl_tuple_emit_functions!(13; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7), (T8, 8), (T9, 9), (T10, 10), (T11, 11), (T12, 12));
impl_tuple_emit_functions!(14; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7), (T8, 8), (T9, 9), (T10, 10), (T11, 11), (T12, 12), (T13, 13));
impl_tuple_emit_functions!(15; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7), (T8, 8), (T9, 9), (T10, 10), (T11, 11), (T12, 12), (T13, 13), (T14, 14));
impl_tuple_emit_functions!(16; (T0, 0), (T1, 1), (T2, 2), (T3, 3), (T4, 4), (T5, 5), (T6, 6), (T7, 7), (T8, 8), (T9, 9), (T10, 10), (T11, 11), (T12, 12), (T13, 13), (T14, 14), (T15, 15));
