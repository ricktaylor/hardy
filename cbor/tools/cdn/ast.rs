/*!
CBOR Diagnostic Notation (CDN) Abstract Syntax Tree

This module defines the AST for CDN, representing all possible CBOR values
in a way that preserves their exact encoding semantics.
*/

use hardy_cbor::encode::{self, Encoder, RuntimeTagged, ToCbor};

/// A CDN value that can be converted to/from CBOR
#[derive(Debug, Clone, PartialEq)]
pub enum CdnValue {
    /// Unsigned integer (CBOR major type 0)
    /// Examples: 0, 42, 1000000
    Unsigned(u64),

    /// Negative integer (CBOR major type 1)
    /// Examples: -1, -42, -1000000
    /// Note: Stored as i64 but encoded as -(n+1) in CBOR
    Negative(i64),

    /// Floating point number (CBOR major type 7, additional info 25/26/27)
    /// Examples: 1.5, -3.14159, 1.0e10
    Float(f64),

    /// Byte string (CBOR major type 2)
    /// CDN syntax: h'deadbeef' or b64'SGVsbG8='
    ByteString(Vec<u8>),

    /// Text string (CBOR major type 3)
    /// CDN syntax: "hello world"
    TextString(String),

    /// Definite-length array (CBOR major type 4)
    /// CDN syntax: [1, 2, 3]
    Array(Vec<CdnValue>),

    /// Indefinite-length array (CBOR major type 4, additional info 31)
    /// CDN syntax: [_ 1, 2, 3]
    ArrayIndefinite(Vec<CdnValue>),

    /// Definite-length map (CBOR major type 5)
    /// CDN syntax: {1: "a", 2: "b"}
    /// Note: Uses Vec to preserve insertion order for round-tripping
    Map(Vec<(CdnValue, CdnValue)>),

    /// Indefinite-length map (CBOR major type 5, additional info 31)
    /// CDN syntax: {_ 1: "a", 2: "b"}
    MapIndefinite(Vec<(CdnValue, CdnValue)>),

    /// Tagged value (CBOR major type 6)
    /// CDN syntax: 24(h'...')
    Tagged(u64, Box<CdnValue>),

    /// Simple value (CBOR major type 7)
    /// CDN syntax: simple(22)
    /// Note: Values 20-23 have special meanings (false, true, null, undefined)
    Simple(u8),

    /// Boolean value (CBOR simple values 20 and 21)
    /// CDN syntax: true, false
    Bool(bool),

    /// Null value (CBOR simple value 22)
    /// CDN syntax: null
    Null,

    /// Undefined value (CBOR simple value 23)
    /// CDN syntax: undefined
    Undefined,
}

impl ToCbor for CdnValue {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        match self {
            CdnValue::Unsigned(n) => {
                encoder.emit(n);
            }
            CdnValue::Negative(n) => {
                encoder.emit(n);
            }
            CdnValue::Float(f) => {
                encoder.emit(f);
            }
            CdnValue::Bool(b) => {
                encoder.emit(b);
            }
            CdnValue::Null => {
                // Emit null as simple value 22 (0xf6)
                encoder.emit(&encode::Raw(&[(7 << 5) | 22]));
            }
            CdnValue::TextString(s) => {
                encoder.emit(s);
            }
            CdnValue::ByteString(bytes) => {
                encoder.emit(&encode::Bytes(bytes.as_slice()));
            }

            CdnValue::Array(items) => {
                // Use built-in slice encoder for definite-length arrays
                encoder.emit(items.as_slice());
            }

            CdnValue::ArrayIndefinite(items) => {
                // Indefinite-length arrays need explicit encoding
                encoder.emit_array(None, |a| {
                    for item in items {
                        a.emit(item);
                    }
                });
            }

            CdnValue::Map(pairs) => {
                encoder.emit_map(Some(pairs.len()), |m| {
                    for (k, v) in pairs {
                        m.emit(k);
                        m.emit(v);
                    }
                });
            }

            CdnValue::MapIndefinite(pairs) => {
                encoder.emit_map(None, |m| {
                    for (k, v) in pairs {
                        m.emit(k);
                        m.emit(v);
                    }
                });
            }

            CdnValue::Tagged(tag, value) => {
                // Use RuntimeTagged for dynamic tag numbers
                encoder.emit(&RuntimeTagged(*tag, value.as_ref()));
            }

            CdnValue::Simple(val) => {
                // Emit simple value directly
                if *val < 24 {
                    encoder.emit(&encode::Raw(&[(7 << 5) | val]));
                } else {
                    encoder.emit(&encode::Raw(&[(7 << 5) | 24, *val]));
                }
            }

            CdnValue::Undefined => {
                // Use Option::None which encodes as CBOR undefined (simple value 23)
                encoder.emit(&Option::<u8>::None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unsigned_integer() {
        let val = CdnValue::Unsigned(42);
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0x18, 0x2a]); // CBOR encoding of 42
    }

    #[test]
    fn test_negative_integer() {
        let val = CdnValue::Negative(-1);
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0x20]); // CBOR encoding of -1
    }

    #[test]
    fn test_text_string() {
        let val = CdnValue::TextString("hello".to_string());
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0x65, b'h', b'e', b'l', b'l', b'o']);
    }

    #[test]
    fn test_byte_string() {
        let val = CdnValue::ByteString(vec![0xde, 0xad, 0xbe, 0xef]);
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0x44, 0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn test_array() {
        let val = CdnValue::Array(vec![
            CdnValue::Unsigned(1),
            CdnValue::Unsigned(2),
            CdnValue::Unsigned(3),
        ]);
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0x83, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn test_map() {
        let val = CdnValue::Map(vec![(
            CdnValue::Unsigned(1),
            CdnValue::TextString("a".to_string()),
        )]);
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0xa1, 0x01, 0x61, b'a']);
    }

    #[test]
    fn test_tagged_value() {
        let val = CdnValue::Tagged(24, Box::new(CdnValue::ByteString(vec![0x01, 0x02])));
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0xd8, 0x18, 0x42, 0x01, 0x02]);
    }

    #[test]
    fn test_bool_true() {
        let val = CdnValue::Bool(true);
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0xf5]); // Simple value 21
    }

    #[test]
    fn test_bool_false() {
        let val = CdnValue::Bool(false);
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0xf4]); // Simple value 20
    }

    #[test]
    fn test_null() {
        let val = CdnValue::Null;
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0xf6]); // Simple value 22
    }

    #[test]
    fn test_undefined() {
        let val = CdnValue::Undefined;
        let cbor = encode::emit(&val).0;
        assert_eq!(cbor, vec![0xf7]); // Simple value 23
    }
}
