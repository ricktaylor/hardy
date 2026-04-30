//! Shared CBOR encoding primitives used by both the buffer and stream codecs.

/// Encode a CBOR head (major type + argument) in canonical shortest form
/// into a stack-allocated buffer. Returns the number of bytes written (1-9).
#[inline]
pub(crate) fn encode_head(buf: &mut [u8; 9], major: u8, value: u64) -> usize {
    match value {
        0..24 => {
            buf[0] = (major << 5) | value as u8;
            1
        }
        24..=0xFF => {
            buf[0] = (major << 5) | 24;
            buf[1] = value as u8;
            2
        }
        0x100..=0xFFFF => {
            buf[0] = (major << 5) | 25;
            buf[1..3].copy_from_slice(&(value as u16).to_be_bytes());
            3
        }
        0x10000..=0xFFFF_FFFF => {
            buf[0] = (major << 5) | 26;
            buf[1..5].copy_from_slice(&(value as u32).to_be_bytes());
            5
        }
        _ => {
            buf[0] = (major << 5) | 27;
            buf[1..9].copy_from_slice(&value.to_be_bytes());
            9
        }
    }
}

/// Encode a float in canonical shortest form (f16 → f32 → f64).
/// Returns the number of bytes written (3, 5, or 9).
#[inline]
pub(crate) fn encode_float_canonical(buf: &mut [u8; 9], v: f64) -> usize {
    let f16_val = half::f16::from_f64(v);
    if f16_val.to_f64() == v {
        buf[0] = (7 << 5) | 25;
        buf[1..3].copy_from_slice(&f16_val.to_be_bytes());
        return 3;
    }
    let f32_val = v as f32;
    if f32_val as f64 == v {
        buf[0] = (7 << 5) | 26;
        buf[1..5].copy_from_slice(&f32_val.to_be_bytes());
        return 5;
    }
    buf[0] = (7 << 5) | 27;
    buf[1..9].copy_from_slice(&v.to_be_bytes());
    9
}
