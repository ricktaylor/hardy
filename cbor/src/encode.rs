use super::*;
use core::ops::Range;

pub trait ToCbor {
    fn to_cbor(&self, encoder: &mut Encoder);
}

pub struct Encoder {
    data: Vec<u8>,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }

    pub fn build(self) -> Vec<u8> {
        self.data
    }

    pub fn offset(&self) -> usize {
        self.data.len()
    }

    fn emit_extend(&mut self, b: &[u8]) -> Range<usize> {
        let start = self.data.len();
        self.data.extend_from_slice(b);
        start..self.data.len()
    }

    fn emit_uint_minor(&mut self, major: u8, val: u64) {
        if val < 24 {
            self.data.push((major << 5) | (val as u8))
        } else if val <= u8::MAX as u64 {
            self.data.push((major << 5) | 24u8);
            self.data.push(val as u8)
        } else if val <= u16::MAX as u64 {
            self.data.push((major << 5) | 25u8);
            self.data.extend((val as u16).to_be_bytes())
        } else if val <= u32::MAX as u64 {
            self.data.push((major << 5) | 26u8);
            self.data.extend((val as u32).to_be_bytes())
        } else {
            self.data.push((major << 5) | 27u8);
            self.data.extend(val.to_be_bytes())
        }
    }

    fn emit_tag(&mut self, tag: u64) -> &mut Self {
        self.emit_uint_minor(6, tag);
        self
    }

    fn emit_tags<I>(&mut self, tags: I) -> &mut Self
    where
        I: IntoIterator<Item = u64>,
    {
        for tag in tags {
            self.emit_tag(tag);
        }
        self
    }

    pub fn emit<T>(&mut self, value: &T)
    where
        T: ToCbor + ?Sized,
    {
        value.to_cbor(self)
    }

    pub fn emit_raw<I>(&mut self, data: I) -> Range<usize>
    where
        I: IntoIterator<Item = u8>,
    {
        let start = self.offset();
        self.data.extend(data);
        start..self.offset()
    }

    fn emit_raw_slice<V>(&mut self, data: &V)
    where
        V: AsRef<[u8]> + ?Sized,
    {
        self.data.extend_from_slice(data.as_ref())
    }

    pub fn emit_bytes<V>(&mut self, value: &V) -> Range<usize>
    where
        V: AsRef<[u8]> + ?Sized,
    {
        let value = value.as_ref();
        self.emit_uint_minor(2, value.len() as u64);
        self.emit_extend(value)
    }

    pub fn emit_bytes_tagged<V, I>(&mut self, value: &V, tags: I) -> Range<usize>
    where
        V: AsRef<[u8]> + ?Sized,
        I: IntoIterator<Item = u64>,
    {
        self.emit_tags(tags).emit_bytes(value)
    }

    pub fn emit_byte_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut ByteStream),
    {
        let mut s = ByteStream::new(self);
        f(&mut s);
        s.end()
    }

    fn emit_string<V>(&mut self, value: &V) -> Range<usize>
    where
        V: AsRef<str> + ?Sized,
    {
        let value = value.as_ref().as_bytes();
        self.emit_uint_minor(3, value.len() as u64);
        self.emit_extend(value)
    }

    pub fn emit_text_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut TextStream),
    {
        let mut s = TextStream::new(self);
        f(&mut s);
        s.end()
    }

    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Array),
    {
        let mut a = Array::new(self, count);
        f(&mut a);
        a.end()
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

    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Map),
    {
        let mut m = Map::new(self, count);
        f(&mut m);
        m.end()
    }
}

/// Marker struct to add a tag value to a type, these can be nested to add multiple tags
pub struct Tagged<'a, const TAG: u64, T>(pub &'a T)
where
    T: ToCbor + ?Sized;

impl<'a, const TAG: u64, T> ToCbor for Tagged<'a, TAG, T>
where
    T: ToCbor + ?Sized,
{
    fn to_cbor(&self, encoder: &mut Encoder) {
        encoder.emit_tag(TAG).emit(self.0)
    }
}

/// Marker struct to ensure that raw bytes are written as raw data, not arrays
pub struct Raw<'a, V>(pub &'a V)
where
    V: AsRef<[u8]> + ?Sized;

impl<'a, V> ToCbor for Raw<'a, V>
where
    V: AsRef<[u8]> + ?Sized,
{
    fn to_cbor(&self, encoder: &mut Encoder) {
        encoder.emit_raw_slice(self.0);
    }
}

/// Marker struct to ensure that byte slices are written as definite length byte streams, not arrays
pub struct Bytes<'a, V>(pub &'a V)
where
    V: AsRef<[u8]> + ?Sized;

impl<'a, V> ToCbor for Bytes<'a, V>
where
    V: AsRef<[u8]> + ?Sized,
{
    fn to_cbor(&self, encoder: &mut Encoder) {
        encoder.emit_bytes(self.0);
    }
}

pub struct ByteStream<'a> {
    encoder: &'a mut Encoder,
}

impl<'a> ByteStream<'a> {
    fn new(encoder: &'a mut Encoder) -> Self {
        encoder.data.push((2 << 5) | 31);
        Self { encoder }
    }

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

pub struct TextStream<'a> {
    encoder: &'a mut Encoder,
}

impl<'a> TextStream<'a> {
    fn new(encoder: &'a mut Encoder) -> Self {
        encoder.data.push((3 << 5) | 31);
        Self { encoder }
    }

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

pub struct Sequence<'a, const D: usize> {
    encoder: &'a mut Encoder,
    start: usize,
    count: Option<usize>,
    idx: usize,
}

pub type Array<'a> = Sequence<'a, 1>;
pub type Map<'a> = Sequence<'a, 2>;

impl<'a, const D: usize> Sequence<'a, D> {
    fn new(encoder: &'a mut Encoder, count: Option<usize>) -> Self {
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

    pub fn offset(&self) -> usize {
        self.encoder.offset() - self.start
    }

    fn next_field(&mut self) -> &mut Encoder {
        self.idx += 1;
        match self.count {
            Some(count) if self.idx > count => {
                panic!("Too many items added to definite length sequence")
            }
            _ => {}
        };
        self.encoder
    }

    fn next_field_tagged<I>(&mut self, tags: I) -> &mut Encoder
    where
        I: IntoIterator<Item = u64>,
    {
        self.next_field().emit_tags(tags)
    }

    fn end(self) {
        let Some(count) = self.count else {
            return self.encoder.data.push(0xFF);
        };
        if self.idx != count {
            panic!(
                "Definite length sequence is short of items: {}, expected {}",
                self.idx, count
            );
        }
    }

    pub fn skip_value(&mut self) {
        self.next_field();
    }

    pub fn emit<T>(&mut self, value: &T)
    where
        T: ToCbor + ?Sized,
    {
        self.next_field().emit(value)
    }

    pub fn emit_raw<I>(&mut self, data: I) -> Range<usize>
    where
        I: IntoIterator<Item = u8>,
    {
        self.next_field().emit_raw(data)
    }

    pub fn emit_bytes<V>(&mut self, value: &V) -> Range<usize>
    where
        V: AsRef<[u8]> + ?Sized,
    {
        self.next_field().emit_bytes(value)
    }

    pub fn emit_bytes_tagged<V, I>(&mut self, value: &V, tags: I) -> Range<usize>
    where
        V: AsRef<[u8]> + ?Sized,
        I: IntoIterator<Item = u64>,
    {
        self.next_field_tagged(tags).emit_bytes(value)
    }

    pub fn emit_byte_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut ByteStream),
    {
        self.next_field().emit_byte_stream(f)
    }

    pub fn emit_text_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut TextStream),
    {
        self.next_field().emit_text_stream(f)
    }

    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Array),
    {
        self.next_field().emit_array(count, f)
    }

    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Map),
    {
        self.next_field().emit_map(count, f)
    }
}

// Blanket implementation for references
impl<T> ToCbor for &T
where
    T: ToCbor,
{
    fn to_cbor(&self, encoder: &mut Encoder) {
        (*self).to_cbor(encoder)
    }
}

macro_rules! impl_uint_to_cbor {
    ($($ty:ty),*) => {
        $(
            impl ToCbor for $ty {
                fn to_cbor(&self, encoder: &mut Encoder) {
                    encoder.emit_uint_minor(0, *self as u64);
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
                fn to_cbor(&self, encoder: &mut Encoder) {
                    if *self >= 0 {
                        encoder.emit_uint_minor(0, *self as u64);
                    } else {
                        encoder.emit_uint_minor(1, self.unsigned_abs() as u64 - 1);
                    }
                }
            }
        )*
    };
}

impl_int_to_cbor!(i8, i16, i32, i64, isize);

fn lossless_float_coerce<T>(value: f64) -> Option<T>
where
    T: num_traits::FromPrimitive + Into<f64> + Copy,
{
    match <T as num_traits::FromPrimitive>::from_f64(value) {
        Some(f) if <T as Into<f64>>::into(f) == value => Some(f),
        _ => None,
    }
}

impl ToCbor for f64 {
    fn to_cbor(&self, encoder: &mut Encoder) {
        if let Some(f) = lossless_float_coerce::<half::f16>(*self) {
            encoder.data.push((7 << 5) | 25);
            encoder.data.extend(f.to_be_bytes())
        } else if let Some(f) = lossless_float_coerce::<f32>(*self) {
            encoder.data.push((7 << 5) | 26);
            encoder.data.extend(f.to_be_bytes())
        } else {
            encoder.data.push((7 << 5) | 27);
            encoder.data.extend(self.to_be_bytes())
        }
    }
}

impl ToCbor for f32 {
    fn to_cbor(&self, encoder: &mut Encoder) {
        if let Some(f) = lossless_float_coerce::<half::f16>(*self as f64) {
            encoder.data.push((7 << 5) | 25);
            encoder.data.extend(f.to_be_bytes())
        } else {
            encoder.data.push((7 << 5) | 26);
            encoder.data.extend(self.to_be_bytes())
        }
    }
}

impl ToCbor for half::f16 {
    fn to_cbor(&self, encoder: &mut Encoder) {
        encoder.data.push((7 << 5) | 25);
        encoder.data.extend(self.to_be_bytes())
    }
}

impl ToCbor for bool {
    fn to_cbor(&self, encoder: &mut Encoder) {
        encoder.data.push((7 << 5) | if *self { 21 } else { 20 })
    }
}

macro_rules! impl_string_to_cbor {
    ($( $value_type:ty),*) => {
        $(
            impl ToCbor for $value_type {
                fn to_cbor(&self, encoder: &mut Encoder) {
                    encoder.emit_string(self);
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
    fn to_cbor(&self, encoder: &mut Encoder) {
        encoder.emit_array_slice(self)
    }
}

impl<T, const N: usize> ToCbor for [T; N]
where
    T: ToCbor,
{
    fn to_cbor(&self, encoder: &mut Encoder) {
        encoder.emit_array_slice(self)
    }
}

impl<T> ToCbor for Option<T>
where
    T: ToCbor,
{
    fn to_cbor(&self, encoder: &mut Encoder) {
        match self {
            Some(value) => encoder.emit(value),
            None => encoder.data.push((7 << 5) | 23),
        }
    }
}

pub fn emit<T>(value: &T) -> Vec<u8>
where
    T: ToCbor + ?Sized,
{
    let mut e = Encoder::new();
    e.emit(value);
    e.build()
}

pub fn emit_bytes<V>(value: &V) -> (Range<usize>, Vec<u8>)
where
    V: AsRef<[u8]> + ?Sized,
{
    let mut e = Encoder::new();
    let r = e.emit_bytes(value);
    (r, e.build())
}

pub fn emit_bytes_tagged<V, I>(value: &V, tags: I) -> (Range<usize>, Vec<u8>)
where
    V: AsRef<[u8]> + ?Sized,
    I: IntoIterator<Item = u64>,
{
    let mut e = Encoder::new();
    let r = e.emit_tags(tags).emit_bytes(value);
    (r, e.build())
}

macro_rules! impl_stream_emit_functions {
    ($(( $method:ident,  $stream_type:ty)),*) => {
        $(
            pub fn $method<F>(f: F) -> Vec<u8>
            where
                F: FnOnce(&mut $stream_type),
            {
                let mut e = Encoder::new();
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
    ($(( $method:ident, $collection_type:ty)),*) => {
        $(
            pub fn $method<F>(count: Option<usize>, f: F) -> Vec<u8>
            where
                F: FnOnce(&mut $collection_type),
            {
                let mut e = Encoder::new();
                e.$method(count, f);
                e.build()
            }
        )*
    };
}

impl_collection_emit_functions!((emit_array, Array), (emit_map, Map));

macro_rules! impl_tuple_emit_functions {
    // The first argument `$len:expr` captures the tuple's length.
    // The `( $($name:ident, $index:tt),* )` part matches a comma-separated list of
    // pairs, like `(T1, 0), (T2, 1)`.
    // `$name` will be the generic type identifier (e.g., T1, T2).
    // `$index` will be the numeric tuple index (e.g., 0, 1).
    ( $len:expr; $( ($name:ident, $index:tt) ),* ) => {
        impl<$($name: ToCbor),*> ToCbor for ($($name,)*) {
            fn to_cbor(&self, encoder: &mut Encoder) {
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

// This is only exposed for testing
#[cfg(test)]
pub(crate) fn emit_simple_value(value: u8) -> Vec<u8> {
    match value {
        20 | 21 | 23 | 24..=31 => panic!("Invalid simple value, use bool or Option<T>"),
        _ => {
            let mut e = Encoder::new();
            e.emit_uint_minor(7, value as u64);
            e.build()
        }
    }
}
