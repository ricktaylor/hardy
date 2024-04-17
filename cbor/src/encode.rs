use num_traits::FromPrimitive;

pub trait ToCbor {
    fn to_cbor(self, encoder: &mut Encoder);
}

pub struct Encoder {
    data: Vec<u8>,
}

impl Encoder {
    fn new() -> Self {
        Self { data: Vec::new() }
    }

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
        T: Into<u64>,
    {
        for tag in tags {
            self.emit_uint_minor(6, tag.into())
        }
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
        T: Into<u64>,
    {
        self.emit_tags(tags);
        self.emit(value)
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
        T: Into<u64>,
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
        T: Into<u64>,
    {
        self.emit_tags(tags);
        self.emit_map(count, f)
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
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
        if let Some(count) = self.count {
            if self.idx >= count {
                panic!("Too many items added to definite length sequence")
            }
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
        T: Into<u64>,
    {
        self.check_bounds();
        self.encoder.emit_tagged(value, tags)
    }

    pub fn emit_array<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Array),
    {
        self.check_bounds();
        let mut a = Array::new(self.encoder, count);
        f(&mut a);
        a.end()
    }

    pub fn emit_array_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F)
    where
        F: FnOnce(&mut Array),
        I: IntoIterator<Item = T>,
        T: Into<u64>,
    {
        self.check_bounds();
        self.encoder.emit_tags(tags);
        let mut a = Array::new(self.encoder, count);
        f(&mut a);
        a.end()
    }

    pub fn emit_map<F>(&mut self, count: Option<usize>, f: F)
    where
        F: FnOnce(&mut Map),
    {
        self.check_bounds();
        let mut m = Map::new(self.encoder, count);
        f(&mut m);
        m.end()
    }

    pub fn emit_map_tagged<F, I, T>(&mut self, count: Option<usize>, tags: I, f: F)
    where
        F: FnOnce(&mut Map),
        I: IntoIterator<Item = T>,
        T: Into<u64>,
    {
        self.check_bounds();
        self.encoder.emit_tags(tags);
        let mut m = Map::new(self.encoder, count);
        f(&mut m);
        m.end()
    }
}

impl ToCbor for u64 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit_uint_minor(0, self)
    }
}

impl ToCbor for usize {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit(self as u64)
    }
}

impl ToCbor for u32 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit::<u64>(self.into())
    }
}

impl ToCbor for u16 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit::<u64>(self.into())
    }
}

impl ToCbor for u8 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit::<u64>(self.into())
    }
}

impl ToCbor for i64 {
    fn to_cbor(self, encoder: &mut Encoder) {
        if self >= 0 {
            encoder.emit_uint_minor(0, self as u64)
        } else {
            encoder.emit_uint_minor(1, i64::abs(self) as u64 + 1)
        }
    }
}

impl ToCbor for isize {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit(self as i64)
    }
}

impl ToCbor for i32 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit::<i64>(self.into())
    }
}

impl ToCbor for i16 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit::<i64>(self.into())
    }
}

impl ToCbor for i8 {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.emit::<i64>(self.into())
    }
}

impl ToCbor for f64 {
    fn to_cbor(self, encoder: &mut Encoder) {
        if let Some(f) = <half::f16 as num_traits::FromPrimitive>::from_f64(self) {
            encoder.data.push((7 << 5) | 25);
            encoder.data.extend(&f.to_be_bytes());
        } else if let Some(f) = f32::from_f64(self) {
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
        encoder.emit::<f64>(self.into())
    }
}

impl ToCbor for bool {
    fn to_cbor(self, encoder: &mut Encoder) {
        encoder.data.push((7 << 5) | if self { 21 } else { 20 })
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

pub fn emit<V>(value: V) -> Vec<u8>
where
    V: ToCbor,
{
    let mut e = Encoder::default();
    e.emit(value);
    e.data
}

pub fn emit_tagged<V, I, T>(value: V, tags: I) -> Vec<u8>
where
    V: ToCbor,
    I: IntoIterator<Item = T>,
    T: Into<u64>,
{
    let mut e = Encoder::default();
    e.emit_tagged(value, tags);
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
    T: Into<u64>,
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
    T: Into<u64>,
{
    let mut e = Encoder::default();
    e.emit_map_tagged(count, tags, f);
    e.data
}
