pub trait ToCbor {
    fn to_cbor(self, encoder: &mut Encoder);

    fn to_cbor_tagged<I, T>(self, encoder: &mut Encoder, tags: I)
    where
        Self: Sized,
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        encoder.emit_tags(tags);
        self.to_cbor(encoder)
    }
}

#[derive(Default)]
pub struct Encoder {
    data: Vec<u8>,
}

impl Encoder {
    fn emit_uint_minor(&mut self, major: u8, val: u64) {
        if val < 24 {
            self.data.push((major << 5) | (val as u8))
        } else if val <= u8::MAX as u64 {
            self.data.push((major << 5) | 24u8);
            self.data.push(val as u8)
        } else if val <= u16::MAX as u64 {
            self.data.push((major << 5) | 25u8);
            self.data.extend(&(val as u16).to_be_bytes())
        } else if val <= u32::MAX as u64 {
            self.data.push((major << 5) | 26u8);
            self.data.extend(&(val as u32).to_be_bytes())
        } else {
            self.data.push((major << 5) | 27u8);
            self.data.extend(&val.to_be_bytes())
        }
    }

    fn emit_tags<I, T>(&mut self, tags: I)
    where
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        for tag in tags {
            self.emit_uint_minor(6, tag.to_u64().expect("Tags must be unsigned integers"))
        }
    }

    pub fn emit_raw(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data)
    }

    pub fn emit<V>(&mut self, value: V)
    where
        V: ToCbor,
    {
        value.to_cbor(self)
    }

    pub fn emit_tagged<V, I, T>(&mut self, value: V, tags: I)
    where
        V: ToCbor,
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.emit_tags(tags);
        self.emit(value)
    }

    pub fn emit_byte_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Stream<&[u8]>),
    {
        let mut s = Stream::new(self, 2);
        f(&mut s);
        s.end()
    }

    pub fn emit_byte_stream_tagged<F, I, T>(&mut self, tags: I, f: F)
    where
        F: FnOnce(&mut Stream<&[u8]>),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.emit_tags(tags);
        self.emit_byte_stream(f)
    }

    pub fn emit_text_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Stream<&str>),
    {
        let mut s = Stream::new(self, 3);
        f(&mut s);
        s.end()
    }

    pub fn emit_text_stream_tagged<F, I, T>(&mut self, tags: I, f: F)
    where
        F: FnOnce(&mut Stream<&str>),
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

pub struct Stream<'a, T>
where
    T: ToCbor,
{
    encoder: &'a mut Encoder,
    phantom: std::marker::PhantomData<T>,
}

impl<'a, T> Stream<'a, T>
where
    T: ToCbor,
{
    fn new(encoder: &'a mut Encoder, major: u8) -> Self {
        encoder.data.push((major << 5) | 31);
        Self {
            encoder,
            phantom: std::marker::PhantomData,
        }
    }

    pub fn emit(&mut self, value: T) {
        self.encoder.emit(value)
    }

    fn end(self) {
        self.encoder.data.push(0xFF)
    }
}

pub struct Sequence<'a, const D: usize> {
    encoder: &'a mut Encoder,
    count: Option<usize>,
    idx: usize,
}

pub type Array<'a> = Sequence<'a, 1>;
pub type Map<'a> = Sequence<'a, 2>;

impl<'a, const D: usize> Sequence<'a, D> {
    fn new(encoder: &'a mut Encoder, count: Option<usize>) -> Self {
        if let Some(count) = count {
            encoder.emit_uint_minor(if D == 1 { 4 } else { 5 }, count as u64);
        } else {
            encoder.data.push((if D == 1 { 4 } else { 5 } << 5) | 31);
        }
        Self {
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

    fn end(self) {
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
    }

    pub fn emit_raw(&mut self, data: &[u8]) {
        self.check_bounds();
        self.encoder.emit_raw(data)
    }

    pub fn emit<V>(&mut self, value: V)
    where
        Self: Sized,
        V: ToCbor,
    {
        self.check_bounds();
        self.encoder.emit(value)
    }

    pub fn emit_tagged<V, I, T>(&mut self, value: V, tags: I)
    where
        V: ToCbor,
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_tagged(value, tags)
    }

    pub fn emit_byte_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Stream<&[u8]>),
    {
        self.check_bounds();
        self.encoder.emit_byte_stream(f)
    }

    pub fn emit_byte_stream_tagged<F, I, T>(&mut self, tags: I, f: F)
    where
        F: FnOnce(&mut Stream<&[u8]>),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_byte_stream_tagged(tags, f)
    }

    pub fn emit_text_stream<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Stream<&str>),
    {
        self.check_bounds();
        self.encoder.emit_text_stream(f)
    }

    pub fn emit_text_stream_tagged<F, I, T>(&mut self, tags: I, f: F)
    where
        F: FnOnce(&mut Stream<&str>),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_text_stream_tagged(tags, f)
    }

    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Array),
    {
        self.check_bounds();
        self.encoder.emit_array(count, f)
    }

    pub fn emit_array_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F)
    where
        F: FnOnce(&mut Array),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_array_tagged(count, tags, f)
    }

    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Map),
    {
        self.check_bounds();
        self.encoder.emit_map(count, f)
    }

    pub fn emit_map_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F)
    where
        F: FnOnce(&mut Map),
        I: IntoIterator<Item = T>,
        T: num_traits::ToPrimitive,
    {
        self.check_bounds();
        self.encoder.emit_map_tagged(count, tags, f)
    }
}

impl ToCbor for u64 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self)
    }
}

impl ToCbor for usize {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self as u64)
    }
}

impl ToCbor for u32 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self as u64)
    }
}

impl ToCbor for u16 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self as u64)
    }
}

impl ToCbor for u8 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self as u64)
    }
}

fn emit_i64(encoder: &mut Encoder, val: i64) {
    if val >= 0 {
        encoder.emit_uint_minor(0, val as u64)
    } else {
        encoder.emit_uint_minor(1, i64::abs(val) as u64 - 1)
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
            encoder.data.extend(&f.to_be_bytes());
        } else if let Some(f) = lossless_float_coerce::<f32>(self) {
            encoder.data.push((7 << 5) | 26);
            encoder.data.extend(&f.to_be_bytes());
        } else {
            encoder.data.push((7 << 5) | 27);
            encoder.data.extend(&self.to_be_bytes());
        }
    }
}

impl ToCbor for f32 {
    fn to_cbor(self, encoder: &mut Encoder) {
        if let Some(f) = lossless_float_coerce::<half::f16>(self as f64) {
            encoder.data.push((7 << 5) | 25);
            encoder.data.extend(&f.to_be_bytes());
        } else {
            encoder.data.push((7 << 5) | 26);
            encoder.data.extend(&self.to_be_bytes());
        }
    }
}

impl ToCbor for half::f16 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.data.push((7 << 5) | 25);
        encoder.data.extend(&self.to_be_bytes());
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

impl ToCbor for &Vec<u8> {
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

impl<V> ToCbor for Option<V>
where
    V: ToCbor,
{
    fn to_cbor(self, encoder: &mut Encoder) {
        match self {
            Some(value) => encoder.emit(value),
            None => encoder.data.push((7 << 5) | 23),
        }
    }
}

impl<V> ToCbor for &Option<V>
where
    for<'a> &'a V: ToCbor,
{
    fn to_cbor(self, encoder: &mut Encoder) {
        match self {
            Some(value) => encoder.emit(value),
            None => encoder.data.push((7 << 5) | 23),
        }
    }
}

pub fn emit<V>(value: V) -> Vec<u8>
where
    V: ToCbor,
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

pub fn emit_tagged<V, I, T>(value: V, tags: I) -> Vec<u8>
where
    V: ToCbor,
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_tagged(value, tags);
    e.data
}

pub fn emit_byte_stream<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut Stream<&[u8]>),
{
    let mut e = Encoder::default();
    e.emit_byte_stream(f);
    e.data
}

pub fn emit_byte_stream_tagged<F, I, T>(tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Stream<&[u8]>),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_byte_stream_tagged(tags, f);
    e.data
}

pub fn emit_text_stream<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut Stream<&str>),
{
    let mut e = Encoder::default();
    e.emit_text_stream(f);
    e.data
}

pub fn emit_text_stream_tagged<F, I, T>(tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Stream<&str>),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_text_stream_tagged(tags, f);
    e.data
}

pub fn emit_array<F>(count: Option<usize>, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Array),
{
    let mut e = Encoder::default();
    e.emit_array(count, f);
    e.data
}

pub fn emit_array_tagged<F, I, T>(count: Option<usize>, tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Array),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_array_tagged(count, tags, f);
    e.data
}

pub fn emit_map<F>(count: Option<usize>, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Map),
{
    let mut e = Encoder::default();
    e.emit_map(count, f);
    e.data
}

pub fn emit_map_tagged<F, I, T>(count: Option<usize>, tags: I, f: F) -> Vec<u8>
where
    F: FnOnce(&mut Map),
    I: IntoIterator<Item = T>,
    T: num_traits::ToPrimitive,
{
    let mut e = Encoder::default();
    e.emit_map_tagged(count, tags, f);
    e.data
}
