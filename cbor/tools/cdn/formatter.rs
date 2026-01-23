/*!
CBOR to CDN formatter

Converts CBOR binary data to CBOR Diagnostic Notation (CDN) text format.
*/

use super::ast::CdnValue;
use hardy_cbor::decode::{self, FromCbor, Value};

/// Format CBOR bytes as CDN text
///
/// # Arguments
///
/// * `data` - The CBOR-encoded bytes to format
/// * `decode_embedded` - If true, opportunistically decode byte strings as nested CBOR
///
/// When `decode_embedded` is true:
/// - Tag 24 byte strings are decoded and shown as `24(decoded_content)`
/// - Untagged byte strings are opportunistically decoded and shown as `<<decoded_content>>`
/// - CBOR sequences (RFC 8742) are shown as `<<item1, item2, ...>>`
/// - Invalid CBOR byte strings fall back to hex notation `h'...'`
/// - Works recursively for nested embedded CBOR
///
/// # Examples
///
/// ```
/// use hardy_cbor_tools::cdn::format_cbor;
///
/// let cbor = vec![0x83, 0x01, 0x02, 0x03]; // [1, 2, 3]
/// let cdn = format_cbor(&cbor, false).unwrap();
/// assert_eq!(cdn, "[1, 2, 3]");
/// ```
pub fn format_cbor(data: &[u8], decode_embedded: bool) -> Result<String, decode::Error> {
    let (value, _shortest, _len) = CdnValue::from_cbor(data)?;
    Ok(format_value(&value, decode_embedded))
}

/// Convert a hardy-cbor Value to CDN AST
///
/// Note: CdnValue is fully owned (no borrowing), so we copy all byte/string data
fn value_to_ast(value: Value, data: &[u8]) -> Result<CdnValue, decode::Error> {
    match value {
        Value::UnsignedInteger(n) => Ok(CdnValue::Unsigned(n)),

        Value::NegativeInteger(n) => {
            // CBOR negative integers are encoded as -(n+1)
            // So we need to convert back to the actual negative value
            let actual = -(n as i128 + 1);
            Ok(CdnValue::Negative(
                actual.try_into().map_err(|_| decode::Error::TooBig)?,
            ))
        }

        Value::Bytes(range) => Ok(CdnValue::ByteString(data[range].to_vec())),

        Value::ByteStream(ranges) => {
            let mut bytes = Vec::new();
            for range in ranges {
                bytes.extend_from_slice(&data[range]);
            }
            Ok(CdnValue::ByteString(bytes))
        }

        Value::Text(s) => Ok(CdnValue::TextString(s.to_string())),

        Value::TextStream(chunks) => {
            let mut text = String::new();
            for chunk in chunks {
                text.push_str(chunk);
            }
            Ok(CdnValue::TextString(text))
        }

        Value::Array(array) => {
            let is_definite = array.is_definite();
            let mut items = Vec::new();

            while let Ok(item) = array.parse::<CdnValue>() {
                items.push(item);
            }

            if is_definite {
                Ok(CdnValue::Array(items))
            } else {
                Ok(CdnValue::ArrayIndefinite(items))
            }
        }

        Value::Map(map) => {
            let is_definite = map.is_definite();
            let mut pairs = Vec::new();

            while let Ok(key) = map.parse::<CdnValue>() {
                let val = map.parse::<CdnValue>()?;
                pairs.push((key, val));
            }

            if is_definite {
                Ok(CdnValue::Map(pairs))
            } else {
                Ok(CdnValue::MapIndefinite(pairs))
            }
        }

        Value::False => Ok(CdnValue::Bool(false)),
        Value::True => Ok(CdnValue::Bool(true)),
        Value::Null => Ok(CdnValue::Null),
        Value::Undefined => Ok(CdnValue::Undefined),
        Value::Simple(n) => Ok(CdnValue::Simple(n)),
        Value::Float(f) => Ok(CdnValue::Float(f)),
    }
}

/// Try to decode a byte string as a CBOR sequence (RFC 8742)
///
/// Returns the formatted string if successful, None if not valid CBOR
fn try_decode_cbor_sequence(bytes: &[u8], decode_embedded: bool) -> Option<String> {
    // Use hardy_cbor's parse_sequence to decode CBOR sequences
    let result = decode::parse_sequence(bytes, |seq| {
        let mut items = Vec::new();

        // Parse all items from the sequence
        while !seq.at_end()? {
            let item = seq.parse::<CdnValue>()?;
            items.push(item);
        }

        Ok::<_, decode::Error>(items)
    });

    match result {
        Ok((items, _len)) => {
            if items.is_empty() {
                None
            } else if items.len() == 1 {
                // Single CBOR item
                Some(format!("<<{}>>", format_value(&items[0], decode_embedded)))
            } else {
                // Multiple items - CBOR sequence
                let formatted: Vec<_> = items
                    .iter()
                    .map(|v| format_value(v, decode_embedded))
                    .collect();
                Some(format!("<<{}>>", formatted.join(", ")))
            }
        }
        Err(_) => {
            // Failed to decode - not valid CBOR
            None
        }
    }
}

/// Format a CDN AST value as text
///
/// # Arguments
///
/// * `value` - The CDN value to format
/// * `decode_embedded` - If true, opportunistically decode byte strings as nested CBOR
///
/// # Embedded CBOR Notation
///
/// When `decode_embedded` is true:
/// - Byte strings that decode as valid CBOR are shown as `<<content>>`
/// - CBOR sequences (multiple items) are shown as `<<item1, item2, ...>>`
/// - Tag 24 byte strings are decoded and shown as `24(content)`
/// - Invalid CBOR byte strings are shown as `h'hexdata'`
fn format_value(value: &CdnValue, decode_embedded: bool) -> String {
    match value {
        CdnValue::Unsigned(n) => n.to_string(),
        CdnValue::Negative(n) => n.to_string(),
        CdnValue::Float(f) => {
            // Format floats with proper handling of special values
            if f.is_nan() {
                "NaN".to_string()
            } else if f.is_infinite() {
                if f.is_sign_positive() {
                    "Infinity".to_string()
                } else {
                    "-Infinity".to_string()
                }
            } else {
                // Use Rust's default float formatting
                format!("{}", f)
            }
        }
        CdnValue::Bool(b) => b.to_string(),
        CdnValue::Null => "null".to_string(),
        CdnValue::Undefined => "undefined".to_string(),

        CdnValue::ByteString(bytes) => {
            // If decode_embedded is enabled, try to decode as CBOR or CBOR sequence
            if decode_embedded {
                match try_decode_cbor_sequence(bytes, decode_embedded) {
                    Some(formatted) => formatted,
                    None => {
                        // Not valid CBOR - show as hex
                        format!("h'{}'", hex::encode(bytes))
                    }
                }
            } else {
                // Decode not enabled - always show as hex
                format!("h'{}'", hex::encode(bytes))
            }
        }

        CdnValue::TextString(s) => {
            // Escape special characters
            format!("\"{}\"", escape_string(s))
        }

        CdnValue::Array(items) => {
            let formatted: Vec<_> = items
                .iter()
                .map(|v| format_value(v, decode_embedded))
                .collect();
            format!("[{}]", formatted.join(", "))
        }

        CdnValue::ArrayIndefinite(items) => {
            let formatted: Vec<_> = items
                .iter()
                .map(|v| format_value(v, decode_embedded))
                .collect();
            format!("[_ {}]", formatted.join(", "))
        }

        CdnValue::Map(pairs) => {
            let formatted: Vec<_> = pairs
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        format_value(k, decode_embedded),
                        format_value(v, decode_embedded)
                    )
                })
                .collect();
            format!("{{{}}}", formatted.join(", "))
        }

        CdnValue::MapIndefinite(pairs) => {
            let formatted: Vec<_> = pairs
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        format_value(k, decode_embedded),
                        format_value(v, decode_embedded)
                    )
                })
                .collect();
            format!("{{_ {}}}", formatted.join(", "))
        }

        CdnValue::Tagged(tag, inner) => {
            // Special handling for tag 24 (embedded CBOR) when decode_embedded is enabled
            if *tag == 24 && decode_embedded {
                if let CdnValue::ByteString(bytes) = inner.as_ref() {
                    // Try to decode the byte string as CBOR or CBOR sequence
                    match try_decode_cbor_sequence(bytes, decode_embedded) {
                        Some(formatted_content) => {
                            // Successfully decoded - extract content from <<...>>
                            // and wrap in tag 24 notation
                            if let Some(content) = formatted_content
                                .strip_prefix("<<")
                                .and_then(|s| s.strip_suffix(">>"))
                            {
                                format!("24({})", content)
                            } else {
                                // Shouldn't happen, but fallback
                                format!("24({})", formatted_content)
                            }
                        }
                        None => {
                            // Failed to decode - fall back to showing as byte string
                            format!("24({})", format_value(inner, decode_embedded))
                        }
                    }
                } else {
                    // Tag 24 but not a byte string - format normally
                    format!("24({})", format_value(inner, decode_embedded))
                }
            } else {
                // Not tag 24, or decode_embedded is false - format normally
                format!("{}({})", tag, format_value(inner, decode_embedded))
            }
        }

        CdnValue::Simple(val) => {
            format!("simple({})", val)
        }
    }
}

/// Escape special characters in strings for CDN output
fn escape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

// Implement FromCbor for CdnValue to enable using parse() in the decoder
impl hardy_cbor::decode::FromCbor for CdnValue {
    type Error = decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        decode::parse_value(data, |val, shortest, tags| {
            let mut result = value_to_ast(val, data)?;

            // Wrap in tags
            for tag in tags.iter().rev() {
                result = CdnValue::Tagged(*tag, Box::new(result));
            }

            Ok((result, shortest))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_unsigned() {
        let cbor = vec![0x18, 0x2a]; // 42
        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "42");
    }

    #[test]
    fn test_format_negative() {
        let cbor = vec![0x20]; // -1
        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "-1");
    }

    #[test]
    fn test_format_text_string() {
        let cbor = vec![0x65, b'h', b'e', b'l', b'l', b'o'];
        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "\"hello\"");
    }

    #[test]
    fn test_format_byte_string() {
        let cbor = vec![0x44, 0xde, 0xad, 0xbe, 0xef];
        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "h'deadbeef'");
    }

    #[test]
    fn test_format_array() {
        let cbor = vec![0x83, 0x01, 0x02, 0x03]; // [1, 2, 3]
        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "[1, 2, 3]");
    }

    #[test]
    fn test_format_map() {
        let cbor = vec![0xa1, 0x01, 0x61, b'a']; // {1: "a"}
        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "{1: \"a\"}");
    }

    #[test]
    fn test_format_tagged() {
        let cbor = vec![0xd8, 0x18, 0x42, 0x01, 0x02]; // 24(h'0102')
        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "24(h'0102')");
    }

    #[test]
    fn test_format_bool() {
        let cbor_true = vec![0xf5];
        let cbor_false = vec![0xf4];
        assert_eq!(format_cbor(&cbor_true, false).unwrap(), "true");
        assert_eq!(format_cbor(&cbor_false, false).unwrap(), "false");
    }

    #[test]
    fn test_format_null() {
        let cbor = vec![0xf6];
        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "null");
    }

    #[test]
    fn test_format_embedded_cbor_disabled() {
        // Tag 24 with embedded CBOR [1, 2, 3]
        let inner_cbor = vec![0x83, 0x01, 0x02, 0x03]; // [1, 2, 3]
        let mut cbor = vec![0xd8, 0x18]; // tag(24)
        cbor.push(0x44); // byte string of length 4
        cbor.extend_from_slice(&inner_cbor);

        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "24(h'83010203')");
    }

    #[test]
    fn test_format_embedded_cbor_enabled() {
        // Tag 24 with embedded CBOR [1, 2, 3]
        let inner_cbor = vec![0x83, 0x01, 0x02, 0x03]; // [1, 2, 3]
        let mut cbor = vec![0xd8, 0x18]; // tag(24)
        cbor.push(0x44); // byte string of length 4
        cbor.extend_from_slice(&inner_cbor);

        let cdn = format_cbor(&cbor, true).unwrap();
        assert_eq!(cdn, "24([1, 2, 3])");
    }

    #[test]
    fn test_format_nested_embedded_cbor() {
        // Tag 24 containing tag 24 containing [1, 2]
        let innermost = vec![0x82, 0x01, 0x02]; // [1, 2]
        let mut inner = vec![0xd8, 0x18, 0x43]; // tag(24) + byte string length 3
        inner.extend_from_slice(&innermost);
        let mut outer = vec![0xd8, 0x18]; // tag(24)
        outer.push(inner.len() as u8 | 0x40); // byte string length
        outer.extend_from_slice(&inner);

        let cdn = format_cbor(&outer, true).unwrap();
        assert_eq!(cdn, "24(24([1, 2]))");
    }

    #[test]
    fn test_format_untagged_embedded_cbor_disabled() {
        // Untagged byte string containing CBOR [1, 2, 3]
        let inner_cbor = vec![0x83, 0x01, 0x02, 0x03]; // [1, 2, 3]
        let mut cbor = vec![0x44]; // byte string of length 4
        cbor.extend_from_slice(&inner_cbor);

        let cdn = format_cbor(&cbor, false).unwrap();
        assert_eq!(cdn, "h'83010203'");
    }

    #[test]
    fn test_format_untagged_embedded_cbor_enabled() {
        // Untagged byte string containing CBOR [1, 2, 3]
        let inner_cbor = vec![0x83, 0x01, 0x02, 0x03]; // [1, 2, 3]
        let mut cbor = vec![0x44]; // byte string of length 4
        cbor.extend_from_slice(&inner_cbor);

        let cdn = format_cbor(&cbor, true).unwrap();
        assert_eq!(cdn, "<<[1, 2, 3]>>");
    }

    #[test]
    fn test_format_untagged_invalid_cbor() {
        // Byte string with invalid CBOR
        let cbor = vec![0x44, 0xde, 0xad, 0xbe, 0xef]; // h'deadbeef'

        let cdn = format_cbor(&cbor, true).unwrap();
        // Should fall back to hex since it's not valid CBOR
        assert_eq!(cdn, "h'deadbeef'");
    }

    #[test]
    fn test_format_array_with_embedded_cbor() {
        // Array containing a byte string with embedded CBOR
        let inner_cbor = vec![0x82, 0x01, 0x02]; // [1, 2]
        let mut cbor = vec![0x81]; // array of length 1
        cbor.push(0x43); // byte string of length 3
        cbor.extend_from_slice(&inner_cbor);

        let cdn = format_cbor(&cbor, true).unwrap();
        assert_eq!(cdn, "[<<[1, 2]>>]");
    }

    #[test]
    fn test_format_cbor_sequence() {
        // Byte string containing CBOR sequence: 1, 2, 3
        let mut cbor_seq = Vec::new();
        cbor_seq.push(0x01); // 1
        cbor_seq.push(0x02); // 2
        cbor_seq.push(0x03); // 3

        let mut cbor = vec![0x43]; // byte string of length 3
        cbor.extend_from_slice(&cbor_seq);

        let cdn = format_cbor(&cbor, true).unwrap();
        assert_eq!(cdn, "<<1, 2, 3>>");
    }

    #[test]
    fn test_format_cbor_sequence_complex() {
        // Byte string containing CBOR sequence: [1, 2], "hello", true
        let mut cbor_seq = Vec::new();
        cbor_seq.extend_from_slice(&[0x82, 0x01, 0x02]); // [1, 2]
        cbor_seq.extend_from_slice(&[0x65, b'h', b'e', b'l', b'l', b'o']); // "hello"
        cbor_seq.push(0xf5); // true

        let mut cbor = vec![cbor_seq.len() as u8 | 0x40]; // byte string length
        cbor.extend_from_slice(&cbor_seq);

        let cdn = format_cbor(&cbor, true).unwrap();
        assert_eq!(cdn, "<<[1, 2], \"hello\", true>>");
    }

    #[test]
    fn test_format_tag24_with_sequence() {
        // Tag 24 containing CBOR sequence
        let mut cbor_seq = Vec::new();
        cbor_seq.push(0x01); // 1
        cbor_seq.push(0x02); // 2

        let mut cbor = vec![0xd8, 0x18]; // tag(24)
        cbor.push(0x42); // byte string of length 2
        cbor.extend_from_slice(&cbor_seq);

        let cdn = format_cbor(&cbor, true).unwrap();
        assert_eq!(cdn, "24(1, 2)");
    }

    #[test]
    fn test_format_array_with_sequence() {
        // Array containing a byte string with CBOR sequence
        let mut cbor_seq = Vec::new();
        cbor_seq.push(0x01); // 1
        cbor_seq.push(0x02); // 2
        cbor_seq.push(0x03); // 3

        let mut cbor = vec![0x81]; // array of length 1
        cbor.push(0x43); // byte string of length 3
        cbor.extend_from_slice(&cbor_seq);

        let cdn = format_cbor(&cbor, true).unwrap();
        assert_eq!(cdn, "[<<1, 2, 3>>]");
    }
}
