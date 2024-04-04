use std::u8;

pub trait ToCbor {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8>;
}

fn emit_uint_minor(major: u8, val: u64) -> Vec<u8> {
    if val < 24 {
        vec![(major << 5) | (val as u8)]
    } else if val <= u8::MAX as u64 {
        vec![(major << 5) | 24u8, val as u8]
    } else if val <= u16::MAX as u64 {
        let mut v = vec![(major << 5) | 25u8];
        v.extend((val as u16).to_be_bytes());
        v
    } else if val <= u32::MAX as u64 {
        let mut v = vec![(major << 5) | 26u8];
        v.extend((val as u32).to_be_bytes());
        v
    } else {
        let mut v = vec![(major << 5) | 27u8];
        v.extend(val.to_be_bytes());
        v
    }
}

pub fn emit_with_tags<T>(value: T, tags: &[u64]) -> Vec<u8>
where
    T: ToCbor,
{
    value.to_cbor(tags)
}

pub fn emit<T>(value: T) -> Vec<u8>
where
    T: ToCbor,
{
    value.to_cbor(&[])
}

pub fn emit_indefinite_array<I, J>(arr: I, tags: &[u64]) -> Vec<u8>
where
    I: IntoIterator<Item = J>,
    J: IntoIterator<Item = u8>,
{
    let mut v = emit_tags(tags);
    v.push((4 << 5) | 31);
    for i in arr {
        v.extend(i);
    }
    v.push(0xFF);
    v
}

fn emit_tags(tags: &[u64]) -> Vec<u8> {
    let mut v = Vec::new();
    for tag in tags {
        v.extend(emit_uint_minor(6, *tag))
    }
    v
}

impl ToCbor for u64 {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(0, self));
        v
    }
}

impl ToCbor for u32 {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(0, self as u64));
        v
    }
}

impl ToCbor for u16 {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(0, self as u64));
        v
    }
}

impl ToCbor for u8 {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(0, self as u64));
        v
    }
}

impl ToCbor for bool {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.push((7 << 5) | if self { 21 } else { 20 });
        v
    }
}

impl ToCbor for usize {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(0, self as u64));
        v
    }
}

impl ToCbor for String {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(3, self.len() as u64));
        v.extend(self.as_bytes());
        v
    }
}

fn to_cbor_bytes<I>(arr: I, len: u64, tags: &[u64]) -> Vec<u8>
where
    I: IntoIterator<Item = u8>,
{
    let mut v = emit_tags(tags);
    v.extend(emit_uint_minor(2, len as u64));
    v.extend(arr);
    v
}

impl ToCbor for Vec<u8> {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let len = self.len() as u64;
        to_cbor_bytes(self, len, tags)
    }
}

impl<const N: usize> ToCbor for [u8; N] {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        to_cbor_bytes(self, N as u64, tags)
    }
}

impl<const N: usize> ToCbor for &[u8; N] {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(2, N as u64));
        v.extend_from_slice(self);
        v
    }
}

impl ToCbor for &[u8] {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(2, self.len() as u64));
        v.extend_from_slice(self);
        v
    }
}

fn to_cbor_array<I, J>(arr: I, len: u64, tags: &[u64]) -> Vec<u8>
where
    I: IntoIterator<Item = J>,
    J: IntoIterator<Item = u8>,
{
    let mut v = emit_tags(tags);
    v.extend(emit_uint_minor(6, len));
    for i in arr {
        v.extend(i);
    }
    v
}

impl ToCbor for Vec<Vec<u8>> {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let len = self.len() as u64;
        to_cbor_array(self, len, tags)
    }
}

impl<const N: usize> ToCbor for Vec<[u8; N]> {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let len = self.len() as u64;
        to_cbor_array(self, len, tags)
    }
}

impl<const N: usize> ToCbor for [Vec<u8>; N] {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        to_cbor_array(self, N as u64, tags)
    }
}

impl<const M: usize, const N: usize> ToCbor for [[u8; N]; M] {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        to_cbor_array(self, M as u64, tags)
    }
}

impl<T> ToCbor for Option<T>
where
    T: ToCbor,
{
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        match self {
            Some(t) => v.extend(t.to_cbor(&[])),
            None => v.push((7 << 5) | 22),
        }
        v
    }
}
