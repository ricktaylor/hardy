fn write_uint_minor(major: u8, val: u64) -> Vec<u8> {
    if val < 24 {
        vec![(major << 5) | (val as u8)]
    } else if val <= u8::MAX as u64 {
        vec![(major << 5) | 24u8, val as u8]
    } else if val <= u16::MAX as u64 {
        vec![(major << 5) | 25u8, (val >> 8) as u8, (val & 0xFF) as u8]
    } else if val <= u32::MAX as u64 {
        vec![
            (major << 5) | 26u8,
            (val >> 24) as u8,
            (val >> 16) as u8,
            (val >> 8) as u8,
            val as u8,
        ]
    } else {
        vec![
            (major << 5) | 27u8,
            (val >> 56) as u8,
            (val >> 48) as u8,
            (val >> 40) as u8,
            (val >> 32) as u8,
            (val >> 24) as u8,
            (val >> 16) as u8,
            (val >> 8) as u8,
            val as u8,
        ]
    }
}

pub fn write_uint(val: u64) -> Vec<u8> {
    write_uint_minor(0, val)
}

pub fn write_array(items: &[Vec<u8>]) -> Vec<u8> {
    let mut v = write_uint_minor(4, items.len() as u64);
    for i in items {
        v.extend(i)
    }
    v
}
