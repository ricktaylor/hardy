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

impl ToCbor for Vec<Vec<u8>> {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = emit_tags(tags);
        v.extend(emit_uint_minor(6, self.len() as u64));
        for i in self {
            v.extend(i);
        }
        v
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
