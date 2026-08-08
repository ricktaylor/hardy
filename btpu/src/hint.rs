use crate::codec::Error;
use alloc::vec::Vec;
use bytes::{BufMut, Bytes, BytesMut};

/// Size of a single hint item header (type+H byte, length byte).
pub const HINT_HEADER_SIZE: usize = 2;

/// Bundle Length hint (Section 9.1).
pub const BUNDLE_LENGTH: u8 = 0;

/// Maximum hint type value: the type occupies the upper 7 bits of the first
/// hint header byte.
pub const MAX_HINT_TYPE: u8 = 0x7F;

/// Maximum hint value length representable by the 8-bit length field.
pub const MAX_HINT_VALUE_LEN: usize = u8::MAX as usize;

/// A decoded BTP-U hint item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintItem {
    /// Bundle Length hint: total length of the bundle being transferred.
    BundleLength(u64),
    /// An unrecognized hint type, preserved for forward compatibility.
    Unknown { hint_type: u8, value: Bytes },
}

/// Returns the total encoded size of a slice of hint items (headers + values).
pub fn encoded_hints_len(hints: &[HintItem]) -> usize {
    hints
        .iter()
        .map(|h| HINT_HEADER_SIZE + hint_value_len(h))
        .sum()
}

/// Check that every hint item is encodable.
///
/// Errors if an [`HintItem::Unknown`] has a type above [`MAX_HINT_TYPE`] or
/// a value longer than [`MAX_HINT_VALUE_LEN`] -- both would otherwise be
/// silently truncated into a corrupt hint chain.
pub fn validate_hints(hints: &[HintItem]) -> Result<(), Error> {
    for item in hints {
        if let HintItem::Unknown { hint_type, value } = item {
            if *hint_type > MAX_HINT_TYPE {
                return Err(Error::InvalidHintType(*hint_type));
            }
            if value.len() > MAX_HINT_VALUE_LEN {
                return Err(Error::HintValueOverflow {
                    length: value.len(),
                    max: MAX_HINT_VALUE_LEN,
                });
            }
        }
    }
    Ok(())
}

/// Encode a chain of hint items into `dst`.
///
/// Sets the H flag on all items except the last, per Section 7.2.
///
/// Errors per [`validate_hints`]; `dst` is untouched on error.  Message
/// encoders should call [`validate_hints`] up front (before writing any
/// header bytes) so the whole destination buffer stays clean on error.
pub fn encode_hints(hints: &[HintItem], dst: &mut BytesMut) -> Result<(), Error> {
    validate_hints(hints)?;

    let count = hints.len();
    for (i, item) in hints.iter().enumerate() {
        let more = i + 1 < count;
        encode_one_hint(item, more, dst);
    }
    Ok(())
}

fn encode_one_hint(item: &HintItem, more: bool, dst: &mut BytesMut) {
    let h_bit: u8 = if more { 1 } else { 0 };
    match item {
        HintItem::BundleLength(len) => {
            let (size, bytes) = encode_bundle_length(*len);
            dst.put_u8((BUNDLE_LENGTH << 1) | h_bit);
            dst.put_u8(size);
            dst.put_slice(&bytes[..size as usize]);
        }
        HintItem::Unknown { hint_type, value } => {
            dst.put_u8((hint_type << 1) | h_bit);
            dst.put_u8(value.len() as u8);
            dst.put_slice(value);
        }
    }
}

/// Encode a bundle length value using the smallest representation.
fn encode_bundle_length(len: u64) -> (u8, [u8; 8]) {
    let mut buf = [0u8; 8];
    if len <= u8::MAX as u64 {
        buf[0] = len as u8;
        (1, buf)
    } else if len <= u16::MAX as u64 {
        let bytes = (len as u16).to_be_bytes();
        buf[..2].copy_from_slice(&bytes);
        (2, buf)
    } else if len <= u32::MAX as u64 {
        let bytes = (len as u32).to_be_bytes();
        buf[..4].copy_from_slice(&bytes);
        (4, buf)
    } else {
        buf = len.to_be_bytes();
        (8, buf)
    }
}

fn hint_value_len(item: &HintItem) -> usize {
    match item {
        HintItem::BundleLength(len) => {
            if *len <= u8::MAX as u64 {
                1
            } else if *len <= u16::MAX as u64 {
                2
            } else if *len <= u32::MAX as u64 {
                4
            } else {
                8
            }
        }
        HintItem::Unknown { value, .. } => value.len(),
    }
}

/// Decode a chain of hint items from `src`.
///
/// `pdu` is the original PDU buffer; `src` must be a subslice of it. Unknown
/// hint values are returned as zero-copy [`Bytes`] views into `pdu`.
///
/// Returns the decoded items and the total number of bytes consumed.
pub fn decode_hints(src: &[u8], pdu: &Bytes) -> Result<(Vec<HintItem>, usize), Error> {
    let mut items = Vec::new();
    let mut offset = 0;

    loop {
        if offset + HINT_HEADER_SIZE > src.len() {
            return Err(Error::InsufficientData {
                needed: offset + HINT_HEADER_SIZE,
                available: src.len(),
            });
        }

        let type_h_byte = src[offset];
        let hint_type = type_h_byte >> 1;
        let more = type_h_byte & 1 != 0;
        let value_len = src[offset + 1] as usize;
        offset += HINT_HEADER_SIZE;

        if offset + value_len > src.len() {
            return Err(Error::InsufficientData {
                needed: offset + value_len,
                available: src.len(),
            });
        }

        let value = &src[offset..offset + value_len];
        offset += value_len;

        let item = decode_hint_item(hint_type, value, pdu)?;
        items.push(item);

        if !more {
            break;
        }
    }

    Ok((items, offset))
}

fn decode_hint_item(hint_type: u8, value: &[u8], pdu: &Bytes) -> Result<HintItem, Error> {
    match hint_type {
        BUNDLE_LENGTH => {
            let len = match value.len() {
                1 => value[0] as u64,
                2 => u16::from_be_bytes([value[0], value[1]]) as u64,
                4 => u32::from_be_bytes([value[0], value[1], value[2], value[3]]) as u64,
                8 => u64::from_be_bytes([
                    value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
                ]),
                n => return Err(Error::InvalidBundleLengthHintSize(n as u8)),
            };
            Ok(HintItem::BundleLength(len))
        }
        _ => Ok(HintItem::Unknown {
            hint_type,
            value: pdu.slice_ref(value),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn oversized_hint_value_rejected() {
        // 256-byte value cannot be represented by the 8-bit length field;
        // it must error rather than truncate into a corrupt chain.
        let hints = vec![HintItem::Unknown {
            hint_type: 0x2A,
            value: Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN + 1]),
        }];
        let mut buf = BytesMut::new();
        let err = encode_hints(&hints, &mut buf).unwrap_err();
        assert!(matches!(err, Error::HintValueOverflow { length: 256, .. }));
        assert!(buf.is_empty());

        // Exactly 255 bytes is fine and round-trips.
        let hints = vec![HintItem::Unknown {
            hint_type: 0x2A,
            value: Bytes::from(vec![0u8; MAX_HINT_VALUE_LEN]),
        }];
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();
        let bytes = buf.freeze();
        let (decoded, _) = decode_hints(&bytes, &bytes).unwrap();
        assert_eq!(decoded, hints);
    }

    #[test]
    fn oversized_hint_type_rejected() {
        // Types above 0x7F would lose their top bit to the << 1 shift.
        let hints = vec![HintItem::Unknown {
            hint_type: 0x80,
            value: Bytes::from_static(b"x"),
        }];
        let mut buf = BytesMut::new();
        let err = encode_hints(&hints, &mut buf).unwrap_err();
        assert!(matches!(err, Error::InvalidHintType(0x80)));
        assert!(buf.is_empty());

        // Exactly 0x7F is fine and round-trips.
        let hints = vec![HintItem::Unknown {
            hint_type: MAX_HINT_TYPE,
            value: Bytes::from_static(b"x"),
        }];
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();
        let bytes = buf.freeze();
        let (decoded, _) = decode_hints(&bytes, &bytes).unwrap();
        assert_eq!(decoded, hints);
    }

    #[test]
    fn round_trip_bundle_length_1byte() {
        let hints = vec![HintItem::BundleLength(200)];
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();
        let bytes = buf.freeze();
        let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, hints);
    }

    #[test]
    fn round_trip_bundle_length_2byte() {
        let hints = vec![HintItem::BundleLength(2000)];
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();
        let bytes = buf.freeze();
        let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, hints);
    }

    #[test]
    fn round_trip_bundle_length_4byte() {
        let hints = vec![HintItem::BundleLength(100_000)];
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();
        let bytes = buf.freeze();
        let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, hints);
    }

    #[test]
    fn round_trip_bundle_length_8byte() {
        let hints = vec![HintItem::BundleLength(u64::MAX)];
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();
        let bytes = buf.freeze();
        let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, hints);
    }

    #[test]
    fn round_trip_chained_hints() {
        let hints = vec![
            HintItem::BundleLength(42),
            HintItem::Unknown {
                hint_type: 5,
                value: Bytes::from_static(b"\x01\x02\x03"),
            },
        ];
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();

        // First hint should have H=1 (more follow)
        assert_eq!(buf[0] & 1, 1);
        // Second hint should have H=0 (last)
        let first_total = HINT_HEADER_SIZE + 1; // BundleLength(42) = 1 byte value
        assert_eq!(buf[first_total] & 1, 0);

        let bytes = buf.freeze();
        let (decoded, consumed) = decode_hints(&bytes, &bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded, hints);
    }

    #[test]
    fn encoded_len_matches_actual() {
        let hints = vec![
            HintItem::BundleLength(2000),
            HintItem::Unknown {
                hint_type: 10,
                value: Bytes::from_static(b"test"),
            },
        ];
        let expected = encoded_hints_len(&hints);
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();
        assert_eq!(buf.len(), expected);
    }

    #[test]
    fn invalid_bundle_length_size() {
        // Manually construct a hint with an invalid 3-byte bundle length
        let data = [
            (BUNDLE_LENGTH << 1), // type=0, H=0
            3,                    // length=3 (invalid)
            0x01,
            0x02,
            0x03, // value
        ];
        let bytes = Bytes::copy_from_slice(&data);
        let result = decode_hints(&bytes, &bytes);
        assert!(result.is_err());
    }

    #[test]
    fn unknown_hint_preserved() {
        let hints = vec![HintItem::Unknown {
            hint_type: 0x7F,
            value: Bytes::from_static(b"\xDE\xAD"),
        }];
        let mut buf = BytesMut::new();
        encode_hints(&hints, &mut buf).unwrap();
        let bytes = buf.freeze();
        let (decoded, _) = decode_hints(&bytes, &bytes).unwrap();
        assert_eq!(decoded, hints);
    }
}
