use alloc::{string::String, vec::Vec};

pub trait ToCbor {
    fn to_cbor(self, encoder: &mut Encoder);

    fn to_cbor_tagged<I, U>(self, encoder: &mut Encoder, tags: I)
    where
        Self: Sized,
        I: IntoIterator<Item = U>,
        U: num_traits::ToPrimitive,
    {
        encoder.emit_tags(tags);
        self.to_cbor(encoder)
    }
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

    fn emit_tags<I, T>(&mut self, tags: I)
    where
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        for tag in tags {
            self.emit_uint_minor(6, tag.to_u64().expect("Tags must be unsigned integers"));
        }
    }

    pub fn emit_raw<I>(&mut self, data: I)
    where
        I: IntoIterator<Item = u8>,
    {
        self.data.extend(data)
    }

    pub fn emit_raw_slice(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data)
    }

    pub fn emit<T>(&mut self, value: T)
    where
        T: ToCbor,
    {
        value.to_cbor(self)
    }

    pub fn emit_tagged<T, I, U>(&mut self, value: T, tags: I)
    where
        T: ToCbor,
        I: IntoIterator<Item = U>,
        U: num_traits::ToPrimitive,
    {
        self.emit_tags(tags);
        self.emit(value)
    }

    pub fn emit_byte_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut ByteStream),
    {
        let mut s = ByteStream::new(self);
        f(&mut s);
        s.end()
    }

    pub fn emit_byte_stream_tagged<F, I, T>(&mut self, tags: I, f: F)
    where
        F: FnOnce(&mut ByteStream),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.emit_tags(tags);
        self.emit_byte_stream(f)
    }

    pub fn emit_text_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut TextStream),
    {
        let mut s = TextStream::new(self);
        f(&mut s);
        s.end()
    }

    pub fn emit_text_stream_tagged<F, I, T>(&mut self, tags: I, f: F)
    where
        F: FnOnce(&mut TextStream),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.emit_tags(tags);
        self.emit_text_stream(f)
    }

    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Array),
    {
        let mut a = Array::new(self, count);
        f(&mut a);
        a.end()
    }

    pub fn emit_array_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F)
    where
        F: FnOnce(&mut Array),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.emit_tags(tags);
        self.emit_array(count, f)
    }

    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Map),
    {
        let mut m = Map::new(self, count);
        f(&mut m);
        m.end()
    }

    pub fn emit_map_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F)
    where
        F: FnOnce(&mut Map),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.emit_tags(tags);
        self.emit_map(count, f)
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

    pub fn emit<V: AsRef<[u8]>>(&mut self, value: V) {
        <&[u8] as ToCbor>::to_cbor(value.as_ref(), self.encoder);
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

    pub fn emit<V: AsRef<str>>(&mut self, value: V) {
        <&str as ToCbor>::to_cbor(value.as_ref(), self.encoder)
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
            encoder.emit_uint_minor(if D == 1 { 4 } else { 5 }, count as u64)
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

    pub fn emit_raw<I>(&mut self, data: I)
    where
        I: IntoIterator<Item = u8>,
    {
        self.next_field().emit_raw(data)
    }

    pub fn emit_raw_slice(&mut self, data: &[u8]) {
        self.next_field().emit_raw_slice(data);
    }

    /// Append an additional slice of data, without incrementing the field count
    pub fn append_raw_slice(&mut self, data: &[u8]) {
        self.encoder.emit_raw_slice(data);
    }

    pub fn emit<T>(&mut self, value: T)
    where
        Self: Sized,
        T: ToCbor,
    {
        self.next_field().emit(value)
    }

    pub fn emit_tagged<T, I, U>(&mut self, value: T, tags: I)
    where
        T: ToCbor,
        I: IntoIterator<Item = U>,
        U: num_traits::ToPrimitive,
    {
        self.next_field().emit_tagged(value, tags)
    }

    pub fn emit_byte_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut ByteStream),
    {
        self.next_field().emit_byte_stream(f)
    }

    pub fn emit_byte_stream_tagged<F, I, T>(&mut self, tags: I, f: F)
    where
        F: FnOnce(&mut ByteStream),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.next_field().emit_byte_stream_tagged(tags, f)
    }

    pub fn emit_text_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut TextStream),
    {
        self.next_field().emit_text_stream(f)
    }

    pub fn emit_text_stream_tagged<F, I, T>(&mut self, tags: I, f: F)
    where
        F: FnOnce(&mut TextStream),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.next_field().emit_text_stream_tagged(tags, f)
    }

    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Array),
    {
        self.next_field().emit_array(count, f)
    }

    pub fn emit_array_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F)
    where
        F: FnOnce(&mut Array),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.next_field().emit_array_tagged(count, tags, f)
    }

    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Map),
    {
        self.next_field().emit_map(count, f)
    }

    pub fn emit_map_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F)
    where
        F: FnOnce(&mut Map),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.next_field().emit_map_tagged(count, tags, f)
    }
}

impl ToCbor for u64 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self);
    }
}

impl ToCbor for usize {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self as u64);
    }
}

impl ToCbor for u32 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self as u64);
    }
}

impl ToCbor for u16 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self as u64);
    }
}

impl ToCbor for u8 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self as u64);
    }
}

fn emit_i64(encoder: &mut Encoder, val: i64) {
    if val >= 0 {
        encoder.emit_uint_minor(0, val as u64);
    } else {
        encoder.emit_uint_minor(1, i64::abs(val) as u64 - 1);
    }
}

impl ToCbor for i64 {
    fn to_cbor(self, encoder: &mut Encoder) {
        emit_i64(encoder, self)
    }
}

impl ToCbor for isize {
    fn to_cbor(self, encoder: &mut Encoder) {
        emit_i64(encoder, self as i64)
    }
}

impl ToCbor for i32 {
    fn to_cbor(self, encoder: &mut Encoder) {
        emit_i64(encoder, self as i64)
    }
}

impl ToCbor for i16 {
    fn to_cbor(self, encoder: &mut Encoder) {
        emit_i64(encoder, self as i64)
    }
}

impl ToCbor for i8 {
    fn to_cbor(self, encoder: &mut Encoder) {
        emit_i64(encoder, self as i64)
    }
}

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
    fn to_cbor(self, encoder: &mut Encoder) {
        if let Some(f) = lossless_float_coerce::<half::f16>(self) {
            encoder.data.push((7 << 5) | 25);
            encoder.data.extend(f.to_be_bytes())
        } else if let Some(f) = lossless_float_coerce::<f32>(self) {
            encoder.data.push((7 << 5) | 26);
            encoder.data.extend(f.to_be_bytes())
        } else {
            encoder.data.push((7 << 5) | 27);
            encoder.data.extend(self.to_be_bytes())
        }
    }
}

impl ToCbor for f32 {
    fn to_cbor(self, encoder: &mut Encoder) {
        if let Some(f) = lossless_float_coerce::<half::f16>(self as f64) {
            encoder.data.push((7 << 5) | 25);
            encoder.data.extend(f.to_be_bytes())
        } else {
            encoder.data.push((7 << 5) | 26);
            encoder.data.extend(self.to_be_bytes())
        }
    }
}

impl ToCbor for half::f16 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.data.push((7 << 5) | 25);
        encoder.data.extend(self.to_be_bytes())
    }
}

impl ToCbor for bool {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.data.push((7 << 5) | if self { 21 } else { 20 })
    }
}

impl ToCbor for String {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(3, self.len() as u64);
        encoder.data.extend(self.as_bytes())
    }
}

impl ToCbor for &str {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(3, self.len() as u64);
        encoder.data.extend(self.as_bytes())
    }
}

impl ToCbor for &[u8] {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(2, self.len() as u64);
        encoder.data.extend(self)
    }
}

impl ToCbor for Vec<u8> {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(2, self.len() as u64);
        encoder.data.extend(self)
    }
}

impl<const N: usize> ToCbor for &[u8; N] {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(2, N as u64);
        encoder.data.extend(self)
    }
}

impl<T> ToCbor for Option<T>
where
    T: ToCbor,
{
    fn to_cbor(self, encoder: &mut Encoder) {
        match self {
            Some(value) => encoder.emit(value),
            None => encoder.data.push((7 << 5) | 23),
        }
    }
}

pub fn emit<T>(value: T) -> Vec<u8>
where
    T: ToCbor,
{
    let mut e = Encoder::new();
    e.emit(value);
    e.build()
}

pub fn emit_simple_value(value: u8) -> Vec<u8> {
    match value {
        20 | 21 | 23 | 24..=31 => panic!("Invalid simple value, use bool or Option<T>"),
        _ => {
            let mut e = Encoder::new();
            e.emit_uint_minor(7, value as u64);
            e.build()
        }
    }
}

pub fn emit_tagged<T, I, U>(value: T, tags: I) -> Vec<u8>
where
    T: ToCbor,
    I: IntoIterator<Item = U>,
    U: num_traits::ToPrimitive,
{
    let mut e = Encoder::new();
    e.emit_tagged(value, tags);
    e.build()
}

pub fn emit_byte_stream<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut ByteStream),
{
    let mut e = Encoder::new();
    e.emit_byte_stream(f);
    e.build()
}

pub fn emit_byte_stream_tagged<F, I, T>(tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut ByteStream),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::new();
    e.emit_byte_stream_tagged(tags, f);
    e.build()
}

pub fn emit_text_stream<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut TextStream),
{
    let mut e = Encoder::new();
    e.emit_text_stream(f);
    e.build()
}

pub fn emit_text_stream_tagged<F, I, T>(tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut TextStream),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::new();
    e.emit_text_stream_tagged(tags, f);
    e.build()
}

pub fn emit_array<F>(count: Option<usize>, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Array),
{
    let mut e = Encoder::new();
    e.emit_array(count, f);
    e.build()
}

pub fn emit_array_tagged<F, I, T>(count: Option<usize>, tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Array),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::new();
    e.emit_array_tagged(count, tags, f);
    e.build()
}

pub fn emit_map<F>(count: Option<usize>, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Map),
{
    let mut e = Encoder::new();
    e.emit_map(count, f);
    e.build()
}

pub fn emit_map_tagged<F, I, T>(count: Option<usize>, tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Map),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::new();
    e.emit_map_tagged(count, tags, f);
    e.build()
}
