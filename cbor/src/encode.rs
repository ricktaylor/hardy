pub trait ToCbor {
    fn to_cbor(self, encoder: &mut Encoder) -> usize;

    fn to_cbor_tagged<I, U>(self, encoder: &mut Encoder, tags: I) -> usize
    where
        Self: Sized,
        I: IntoIterator<Item = U>,
        U: num_traits::ToPrimitive,
    {
        let len = encoder.emit_tags(tags);
        self.to_cbor(encoder) + len
    }
}

#[derive(Default)]
pub struct Encoder {
    data: Vec<u8>,
}

impl Encoder {
    fn emit_uint_minor(&mut self, major: u8, val: u64) -> usize {
        if val < 24 {
            self.data.push((major << 5) | (val as u8));
            1
        } else if val <= u8::MAX as u64 {
            self.data.push((major << 5) | 24u8);
            self.data.push(val as u8);
            2
        } else if val <= u16::MAX as u64 {
            self.data.push((major << 5) | 25u8);
            self.data.extend(&(val as u16).to_be_bytes());
            3
        } else if val <= u32::MAX as u64 {
            self.data.push((major << 5) | 26u8);
            self.data.extend(&(val as u32).to_be_bytes());
            5
        } else {
            self.data.push((major << 5) | 27u8);
            self.data.extend(&val.to_be_bytes());
            9
        }
    }

    fn emit_tags<I, T>(&mut self, tags: I) -> usize
    where
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        let mut len = 0;
        for tag in tags {
            len += self.emit_uint_minor(6, tag.to_u64().expect("Tags must be unsigned integers"))
        }
        len
    }

    pub fn emit_raw(&mut self, data: &[u8]) -> usize {
        self.data.extend_from_slice(data);
        data.len()
    }

    pub fn emit<T>(&mut self, value: T) -> usize
    where
        T: ToCbor,
    {
        value.to_cbor(self)
    }

    pub fn emit_tagged<T, I, U>(&mut self, value: T, tags: I) -> usize
    where
        T: ToCbor,
        I: IntoIterator<Item = U>,
        U: num_traits::ToPrimitive,
    {
        let len = self.emit_tags(tags);
        self.emit(value) + len
    }

    pub fn emit_byte_stream<F>(&mut self, f: F) -> usize
    where
        F: FnOnce(&mut ByteStream),
    {
        let mut s = ByteStream::new(self);
        f(&mut s);
        s.end()
    }

    pub fn emit_byte_stream_tagged<F, I, T>(&mut self, tags: I, f: F) -> usize
    where
        F: FnOnce(&mut ByteStream),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        let len = self.emit_tags(tags);
        self.emit_byte_stream(f) + len
    }

    pub fn emit_text_stream<F>(&mut self, f: F) -> usize
    where
        F: FnOnce(&mut TextStream),
    {
        let mut s = TextStream::new(self);
        f(&mut s);
        s.end()
    }

    pub fn emit_text_stream_tagged<F, I, T>(&mut self, tags: I, f: F) -> usize
    where
        F: FnOnce(&mut TextStream),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        let len = self.emit_tags(tags);
        self.emit_text_stream(f) + len
    }

    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F) -> usize
    where
        F: FnOnce(&mut Array, usize),
    {
        let mut a = Array::new(self, count);
        let offset = a.encoder.data.len();
        f(&mut a, offset);
        a.end()
    }

    pub fn emit_array_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F) -> usize
    where
        F: FnOnce(&mut Array, usize),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        let len = self.emit_tags(tags);
        self.emit_array(count, f) + len
    }

    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F) -> usize
    where
        F: FnOnce(&mut Map, usize),
    {
        let mut m = Map::new(self, count);
        let offset = m.encoder.data.len();
        f(&mut m, offset);
        m.end()
    }

    pub fn emit_map_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F) -> usize
    where
        F: FnOnce(&mut Map, usize),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        let len = self.emit_tags(tags);
        self.emit_map(count, f) + len
    }
}

pub struct ByteStream<'a> {
    encoder: &'a mut Encoder,
    offset: usize,
}

impl<'a> ByteStream<'a> {
    fn new(encoder: &'a mut Encoder) -> Self {
        encoder.data.push((2 << 5) | 31);
        Self {
            offset: encoder.data.len() - 1,
            encoder,
        }
    }

    pub fn emit<V: AsRef<[u8]>>(&mut self, value: V) {
        <&[u8] as ToCbor>::to_cbor(value.as_ref(), self.encoder);
    }

    fn end(self) -> usize {
        self.encoder.data.push(0xFF);
        self.encoder.data.len() - self.offset
    }
}

pub struct TextStream<'a> {
    encoder: &'a mut Encoder,
    offset: usize,
}

impl<'a> TextStream<'a> {
    fn new(encoder: &'a mut Encoder) -> Self {
        encoder.data.push((3 << 5) | 31);
        Self {
            offset: encoder.data.len() - 1,
            encoder,
        }
    }

    pub fn emit<V: AsRef<str>>(&mut self, value: V) {
        <&str as ToCbor>::to_cbor(value.as_ref(), self.encoder);
    }

    fn end(self) -> usize {
        self.encoder.data.push(0xFF);
        self.encoder.data.len() - self.offset
    }
}

pub struct Sequence<'a, const D: usize> {
    encoder: &'a mut Encoder,
    offset: usize,
    count: Option<usize>,
    idx: usize,
}

pub type Array<'a> = Sequence<'a, 1>;
pub type Map<'a> = Sequence<'a, 2>;

impl<'a, const D: usize> Sequence<'a, D> {
    fn new(encoder: &'a mut Encoder, count: Option<usize>) -> Self {
        let len = if let Some(count) = count {
            encoder.emit_uint_minor(if D == 1 { 4 } else { 5 }, count as u64)
        } else {
            encoder.data.push((if D == 1 { 4 } else { 5 } << 5) | 31);
            1
        };
        Self {
            offset: encoder.data.len() - len,
            encoder,
            count: count.map(|c| c * D),
            idx: 0,
        }
    }

    fn check_bounds(&mut self) {
        self.idx += 1;
        match self.count {
            Some(count) if self.idx > count => {
                panic!("Too many items added to definite length sequence")
            }
            _ => (),
        }
    }

    fn end(self) -> usize {
        match self.count {
            Some(count) => {
                if self.idx != count {
                    panic!(
                        "Definite length sequence is short of items: {}, expected {}",
                        self.idx, count
                    );
                }
            }
            None => self.encoder.data.push(0xFF),
        }
        self.encoder.data.len() - self.offset
    }

    pub fn skip_value(&mut self) {
        self.check_bounds()
    }

    pub fn emit_raw(&mut self, data: &[u8]) -> usize {
        self.check_bounds();
        self.encoder.emit_raw(data)
    }

    pub fn emit<T>(&mut self, value: T) -> usize
    where
        Self: Sized,
        T: ToCbor,
    {
        self.check_bounds();
        self.encoder.emit(value)
    }

    pub fn emit_tagged<T, I, U>(&mut self, value: T, tags: I) -> usize
    where
        T: ToCbor,
        I: IntoIterator<Item = U>,
        U: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_tagged(value, tags)
    }

    pub fn emit_byte_stream<F>(&mut self, f: F) -> usize
    where
        F: FnOnce(&mut ByteStream),
    {
        self.check_bounds();
        self.encoder.emit_byte_stream(f)
    }

    pub fn emit_byte_stream_tagged<F, I, T>(&mut self, tags: I, f: F) -> usize
    where
        F: FnOnce(&mut ByteStream),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_byte_stream_tagged(tags, f)
    }

    pub fn emit_text_stream<F>(&mut self, f: F) -> usize
    where
        F: FnOnce(&mut TextStream),
    {
        self.check_bounds();
        self.encoder.emit_text_stream(f)
    }

    pub fn emit_text_stream_tagged<F, I, T>(&mut self, tags: I, f: F) -> usize
    where
        F: FnOnce(&mut TextStream),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_text_stream_tagged(tags, f)
    }

    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F) -> usize
    where
        F: FnOnce(&mut Array, usize),
    {
        self.check_bounds();
        self.encoder.emit_array(count, f)
    }

    pub fn emit_array_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F) -> usize
    where
        F: FnOnce(&mut Array, usize),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_array_tagged(count, tags, f)
    }

    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F) -> usize
    where
        F: FnOnce(&mut Map, usize),
    {
        self.check_bounds();
        self.encoder.emit_map(count, f)
    }

    pub fn emit_map_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F) -> usize
    where
        F: FnOnce(&mut Map, usize),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_map_tagged(count, tags, f)
    }
}

impl ToCbor for u64 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        encoder.emit_uint_minor(0, self)
    }
}

impl ToCbor for usize {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        encoder.emit_uint_minor(0, self as u64)
    }
}

impl ToCbor for u32 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        encoder.emit_uint_minor(0, self as u64)
    }
}

impl ToCbor for u16 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        encoder.emit_uint_minor(0, self as u64)
    }
}

impl ToCbor for u8 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        encoder.emit_uint_minor(0, self as u64)
    }
}

fn emit_i64(encoder: &mut Encoder, val: i64) -> usize {
    if val >= 0 {
        encoder.emit_uint_minor(0, val as u64)
    } else {
        encoder.emit_uint_minor(1, i64::abs(val) as u64 - 1)
    }
}

impl ToCbor for i64 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        emit_i64(encoder, self)
    }
}

impl ToCbor for isize {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        emit_i64(encoder, self as i64)
    }
}

impl ToCbor for i32 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        emit_i64(encoder, self as i64)
    }
}

impl ToCbor for i16 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        emit_i64(encoder, self as i64)
    }
}

impl ToCbor for i8 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
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
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        if let Some(f) = lossless_float_coerce::<half::f16>(self) {
            encoder.data.push((7 << 5) | 25);
            let v = f.to_be_bytes();
            let len = v.len() + 1;
            encoder.data.extend(v);
            len
        } else if let Some(f) = lossless_float_coerce::<f32>(self) {
            encoder.data.push((7 << 5) | 26);
            let v = f.to_be_bytes();
            let len = v.len() + 1;
            encoder.data.extend(v);
            len
        } else {
            encoder.data.push((7 << 5) | 27);
            let v = self.to_be_bytes();
            let len = v.len() + 1;
            encoder.data.extend(v);
            len
        }
    }
}

impl ToCbor for f32 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        if let Some(f) = lossless_float_coerce::<half::f16>(self as f64) {
            encoder.data.push((7 << 5) | 25);
            let v = f.to_be_bytes();
            let len = v.len() + 1;
            encoder.data.extend(v);
            len
        } else {
            encoder.data.push((7 << 5) | 26);
            let v = self.to_be_bytes();
            let len = v.len() + 1;
            encoder.data.extend(v);
            len
        }
    }
}

impl ToCbor for half::f16 {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        encoder.data.push((7 << 5) | 25);
        let v = self.to_be_bytes();
        let len = v.len() + 1;
        encoder.data.extend(v);
        len
    }
}

impl ToCbor for bool {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        encoder.data.push((7 << 5) | if self { 21 } else { 20 });
        1
    }
}

impl ToCbor for String {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        let len = encoder.emit_uint_minor(3, self.len() as u64) + self.len();
        encoder.data.extend(self.as_bytes());
        len
    }
}

impl ToCbor for &str {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        let len = encoder.emit_uint_minor(3, self.len() as u64) + self.len();
        encoder.data.extend(self.as_bytes());
        len
    }
}

impl ToCbor for &[u8] {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        let len = encoder.emit_uint_minor(2, self.len() as u64) + self.len();
        encoder.data.extend(self);
        len
    }
}

impl ToCbor for Vec<u8> {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        let len = encoder.emit_uint_minor(2, self.len() as u64) + self.len();
        encoder.data.extend(self);
        len
    }
}

impl<const N: usize> ToCbor for &[u8; N] {
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        let len = encoder.emit_uint_minor(2, N as u64) + N;
        encoder.data.extend(self);
        len
    }
}

impl<T> ToCbor for Option<T>
where
    T: ToCbor,
{
    fn to_cbor(self, encoder: &mut Encoder) -> usize {
        match self {
            Some(value) => encoder.emit(value),
            None => {
                encoder.data.push((7 << 5) | 23);
                1
            }
        }
    }
}

pub fn emit<T>(value: T) -> Vec<u8>
where
    T: ToCbor,
{
    let mut e = Encoder::default();
    e.emit(value);
    e.data
}

pub fn emit_simple_value(value: u8) -> Vec<u8> {
    match value {
        20 | 21 | 23 | 24..=31 => panic!("Invalid simple value, use bool or Option<T>"),
        _ => {
            let mut e = Encoder::default();
            e.emit_uint_minor(7, value as u64);
            e.data
        }
    }
}

pub fn emit_tagged<T, I, U>(value: T, tags: I) -> Vec<u8>
where
    T: ToCbor,
    I: IntoIterator<Item = U>,
    U: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_tagged(value, tags);
    e.data
}

pub fn emit_byte_stream<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut ByteStream),
{
    let mut e = Encoder::default();
    e.emit_byte_stream(f);
    e.data
}

pub fn emit_byte_stream_tagged<F, I, T>(tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut ByteStream),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_byte_stream_tagged(tags, f);
    e.data
}

pub fn emit_text_stream<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut TextStream),
{
    let mut e = Encoder::default();
    e.emit_text_stream(f);
    e.data
}

pub fn emit_text_stream_tagged<F, I, T>(tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut TextStream),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_text_stream_tagged(tags, f);
    e.data
}

pub fn emit_array<F>(count: Option<usize>, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Array, usize),
{
    let mut e = Encoder::default();
    e.emit_array(count, f);
    e.data
}

pub fn emit_array_tagged<F, I, T>(count: Option<usize>, tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Array, usize),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_array_tagged(count, tags, f);
    e.data
}

pub fn emit_map<F>(count: Option<usize>, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Map, usize),
{
    let mut e = Encoder::default();
    e.emit_map(count, f);
    e.data
}

pub fn emit_map_tagged<F, I, T>(count: Option<usize>, tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Map, usize),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_map_tagged(count, tags, f);
    e.data
}
