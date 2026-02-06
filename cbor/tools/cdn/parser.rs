/*!
CDN parser using Chumsky 0.12

Parses CBOR Diagnostic Notation (CDN) text into AST.
*/

use super::ast::CdnValue;
use base64::prelude::*;
use chumsky::prelude::*;

// ============================================================================
// Type Aliases
// ============================================================================

type Span = SimpleSpan<usize>;
type Extra<'a> = extra::Err<Rich<'a, char, Span>>;
type BoxedParser<'a, T> = Boxed<'a, 'a, &'a str, T, Extra<'a>>;

// ============================================================================
// Public API
// ============================================================================

/// Parse CDN text into a CDN AST value
///
/// # Examples
///
/// ```
/// use hardy_cbor_tools::cdn::parse;
///
/// let cdn = "[1, 2, h'deadbeef']";
/// let value = parse(cdn).unwrap();
/// ```
pub fn parse(input: &str) -> Result<CdnValue, Vec<Rich<'_, char, Span>>> {
    cdn_parser().parse(input).into_result()
}

// ============================================================================
// Parser Implementation
// ============================================================================

/// Build the complete CDN parser
fn cdn_parser<'a>() -> BoxedParser<'a, CdnValue> {
    value_parser().then_ignore(end()).boxed()
}

/// Parse whitespace
fn whitespace<'a>() -> BoxedParser<'a, ()> {
    any()
        .filter(|c: &char| c.is_whitespace())
        .repeated()
        .ignored()
        .boxed()
}

/// Parse a single CDN value (recursive)
fn value_parser<'a>() -> BoxedParser<'a, CdnValue> {
    recursive(|value| {
        let value_boxed: BoxedParser<'a, CdnValue> = value.clone().boxed();

        // Combine all value parsers
        // Order matters: try more specific patterns first
        choice((
            tagged_parser(value_boxed.clone()),
            bool_true_parser(),
            bool_false_parser(),
            null_parser(),
            undefined_parser(),
            simple_parser(),
            float_parser(),
            negative_parser(),
            unsigned_parser(),
            hex_bytes_parser(),
            b64_bytes_parser(),
            text_string_parser(),
            array_parser(value_boxed.clone()),
            map_parser(value_boxed),
        ))
        .padded_by(whitespace())
    })
    .boxed()
}

/// Unsigned integer: 0, 42, 1000000
fn unsigned_parser<'a>() -> BoxedParser<'a, CdnValue> {
    text::int(10)
        .try_map(|s: &str, span| {
            s.parse::<u64>()
                .map(CdnValue::Unsigned)
                .map_err(|e| Rich::custom(span, format!("Invalid unsigned integer: {}", e)))
        })
        .labelled("unsigned integer")
        .boxed()
}

/// Negative integer: -1, -42, -1000000
fn negative_parser<'a>() -> BoxedParser<'a, CdnValue> {
    just('-')
        .ignore_then(text::int(10))
        .try_map(|s: &str, span| {
            s.parse::<i64>()
                .map(|n| CdnValue::Negative(-n))
                .map_err(|e| Rich::custom(span, format!("Invalid negative integer: {}", e)))
        })
        .labelled("negative integer")
        .boxed()
}

/// Float: 1.5, -3.14159, 1.0e10
fn float_parser<'a>() -> BoxedParser<'a, CdnValue> {
    let sign = just('-').or_not();
    let integer = text::int(10);
    let fraction = just('.').then(text::digits(10)).ignored();
    let exponent = one_of("eE")
        .then(one_of("+-").or_not())
        .then(text::int(10))
        .ignored();

    // Float requires at least a fraction or exponent part
    sign.then(integer)
        .then(fraction.or(exponent).repeated().at_least(1))
        .to_slice()
        .try_map(|s: &str, span| {
            s.parse::<f64>()
                .map(CdnValue::Float)
                .map_err(|e| Rich::custom(span, format!("Invalid float: {}", e)))
        })
        .labelled("float")
        .boxed()
}

/// Hex byte string: h'deadbeef'
fn hex_bytes_parser<'a>() -> BoxedParser<'a, CdnValue> {
    just("h'")
        .ignore_then(
            any()
                .filter(|c: &char| c.is_ascii_hexdigit())
                .repeated()
                .collect::<String>(),
        )
        .then_ignore(just('\''))
        .try_map(|hex_str, span| {
            hex::decode(&hex_str)
                .map(CdnValue::ByteString)
                .map_err(|e| Rich::custom(span, format!("Invalid hex string: {}", e)))
        })
        .labelled("hex byte string")
        .boxed()
}

/// Base64 byte string: b64'SGVsbG8='
fn b64_bytes_parser<'a>() -> BoxedParser<'a, CdnValue> {
    just("b64'")
        .ignore_then(
            any()
                .filter(|c: &char| *c != '\'')
                .repeated()
                .collect::<String>(),
        )
        .then_ignore(just('\''))
        .try_map(|b64_str, span| {
            BASE64_URL_SAFE_NO_PAD
                .decode(&b64_str)
                .map(CdnValue::ByteString)
                .map_err(|e| Rich::custom(span, format!("Invalid base64 string: {}", e)))
        })
        .labelled("base64 byte string")
        .boxed()
}

/// Text string: "hello world"
fn text_string_parser<'a>() -> BoxedParser<'a, CdnValue> {
    // Parse escape sequences
    let escape = just('\\').ignore_then(any()).map(|c| format!("\\{}", c));

    let normal = none_of("\"\\").map(|c: char| c.to_string());

    just('"')
        .ignore_then(escape.or(normal).repeated().collect::<Vec<String>>())
        .then_ignore(just('"'))
        .map(|parts| {
            let s: String = parts.into_iter().collect();
            CdnValue::TextString(unescape_string(&s))
        })
        .labelled("text string")
        .boxed()
}

/// Array: [1, 2, 3] or [_ 1, 2, 3]
fn array_parser<'a>(value: BoxedParser<'a, CdnValue>) -> BoxedParser<'a, CdnValue> {
    just('[')
        .padded_by(whitespace())
        .ignore_then(just('_').padded_by(whitespace()).or_not())
        .then(
            value
                .separated_by(just(',').padded_by(whitespace()))
                .allow_trailing()
                .collect::<Vec<_>>()
                .padded_by(whitespace()),
        )
        .then_ignore(just(']').padded_by(whitespace()))
        .map(|(indefinite, items): (Option<char>, Vec<CdnValue>)| {
            if indefinite.is_some() {
                CdnValue::ArrayIndefinite(items)
            } else {
                CdnValue::Array(items)
            }
        })
        .labelled("array")
        .boxed()
}

/// Map: {1: "a", 2: "b"} or {_ 1: "a"}
fn map_parser<'a>(value: BoxedParser<'a, CdnValue>) -> BoxedParser<'a, CdnValue> {
    let map_entry = value
        .clone()
        .then_ignore(just(':').padded_by(whitespace()))
        .then(value)
        .boxed();

    just('{')
        .padded_by(whitespace())
        .ignore_then(just('_').padded_by(whitespace()).or_not())
        .then(
            map_entry
                .separated_by(just(',').padded_by(whitespace()))
                .allow_trailing()
                .collect::<Vec<_>>()
                .padded_by(whitespace()),
        )
        .then_ignore(just('}').padded_by(whitespace()))
        .map(
            |(indefinite, pairs): (Option<char>, Vec<(CdnValue, CdnValue)>)| {
                if indefinite.is_some() {
                    CdnValue::MapIndefinite(pairs)
                } else {
                    CdnValue::Map(pairs)
                }
            },
        )
        .labelled("map")
        .boxed()
}

/// Tagged value: 24(h'...')
fn tagged_parser<'a>(value: BoxedParser<'a, CdnValue>) -> BoxedParser<'a, CdnValue> {
    text::int(10)
        .try_map(|s: &str, span| {
            s.parse::<u64>()
                .map_err(|e| Rich::custom(span, format!("Invalid tag number: {}", e)))
        })
        .then_ignore(just('(').padded_by(whitespace()))
        .then(value)
        .then_ignore(just(')').padded_by(whitespace()))
        .map(|(tag, val)| CdnValue::Tagged(tag, Box::new(val)))
        .labelled("tagged value")
        .boxed()
}

/// Boolean true
fn bool_true_parser<'a>() -> BoxedParser<'a, CdnValue> {
    text::keyword("true")
        .to(CdnValue::Bool(true))
        .labelled("true")
        .boxed()
}

/// Boolean false
fn bool_false_parser<'a>() -> BoxedParser<'a, CdnValue> {
    text::keyword("false")
        .to(CdnValue::Bool(false))
        .labelled("false")
        .boxed()
}

/// Null value
fn null_parser<'a>() -> BoxedParser<'a, CdnValue> {
    text::keyword("null")
        .to(CdnValue::Null)
        .labelled("null")
        .boxed()
}

/// Undefined value
fn undefined_parser<'a>() -> BoxedParser<'a, CdnValue> {
    text::keyword("undefined")
        .to(CdnValue::Undefined)
        .labelled("undefined")
        .boxed()
}

/// Simple value: simple(22)
fn simple_parser<'a>() -> BoxedParser<'a, CdnValue> {
    text::keyword("simple")
        .ignore_then(just('(').padded_by(whitespace()))
        .ignore_then(text::int(10))
        .then_ignore(just(')').padded_by(whitespace()))
        .try_map(|s: &str, span| {
            s.parse::<u8>()
                .map(CdnValue::Simple)
                .map_err(|e| Rich::custom(span, format!("Invalid simple value: {}", e)))
        })
        .labelled("simple value")
        .boxed()
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Unescape a string (handle \n, \t, \", etc.)
fn unescape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some('/') => result.push('/'),
                Some('b') => result.push('\x08'),
                Some('f') => result.push('\x0C'),
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('u') => {
                    // Unicode escape: \uXXXX
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Ok(code) = u32::from_str_radix(&hex, 16)
                        && let Some(ch) = char::from_u32(code)
                    {
                        result.push(ch);
                    }
                }
                Some(other) => {
                    // Unknown escape, keep as-is
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }

    result
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_unsigned() {
        let value = parse("42").unwrap();
        assert_eq!(value, CdnValue::Unsigned(42));
    }

    #[test]
    fn test_parse_negative() {
        let value = parse("-42").unwrap();
        assert_eq!(value, CdnValue::Negative(-42));
    }

    #[test]
    fn test_parse_float() {
        let value = parse("3.14159").unwrap();
        match value {
            CdnValue::Float(f) => assert!((f - 3.14159).abs() < 0.00001),
            _ => panic!("Expected float"),
        }
    }

    #[test]
    fn test_parse_text_string() {
        let value = parse(r#""hello world""#).unwrap();
        assert_eq!(value, CdnValue::TextString("hello world".to_string()));
    }

    #[test]
    fn test_parse_hex_bytes() {
        let value = parse("h'deadbeef'").unwrap();
        assert_eq!(value, CdnValue::ByteString(vec![0xde, 0xad, 0xbe, 0xef]));
    }

    #[test]
    fn test_parse_array() {
        let value = parse("[1, 2, 3]").unwrap();
        assert_eq!(
            value,
            CdnValue::Array(vec![
                CdnValue::Unsigned(1),
                CdnValue::Unsigned(2),
                CdnValue::Unsigned(3),
            ])
        );
    }

    #[test]
    fn test_parse_map() {
        let value = parse(r#"{1: "a", 2: "b"}"#).unwrap();
        assert_eq!(
            value,
            CdnValue::Map(vec![
                (CdnValue::Unsigned(1), CdnValue::TextString("a".to_string())),
                (CdnValue::Unsigned(2), CdnValue::TextString("b".to_string())),
            ])
        );
    }

    #[test]
    fn test_parse_tagged() {
        let value = parse("24(h'0102')").unwrap();
        assert_eq!(
            value,
            CdnValue::Tagged(24, Box::new(CdnValue::ByteString(vec![0x01, 0x02])))
        );
    }

    #[test]
    fn test_parse_bool() {
        assert_eq!(parse("true").unwrap(), CdnValue::Bool(true));
        assert_eq!(parse("false").unwrap(), CdnValue::Bool(false));
    }

    #[test]
    fn test_parse_null() {
        assert_eq!(parse("null").unwrap(), CdnValue::Null);
    }

    #[test]
    fn test_parse_undefined() {
        assert_eq!(parse("undefined").unwrap(), CdnValue::Undefined);
    }

    #[test]
    fn test_parse_indefinite_array() {
        let value = parse("[_ 1, 2, 3]").unwrap();
        assert_eq!(
            value,
            CdnValue::ArrayIndefinite(vec![
                CdnValue::Unsigned(1),
                CdnValue::Unsigned(2),
                CdnValue::Unsigned(3),
            ])
        );
    }

    #[test]
    fn test_roundtrip() {
        let cdn = r#"24([1, "hello", h'deadbeef'])"#;
        let parsed = parse(cdn).unwrap();
        let cbor = hardy_cbor::encode::emit(&parsed).0;
        let formatted = crate::cdn::format_cbor(&cbor, false).unwrap();

        // Parse both and compare (since formatting might differ slightly)
        let parsed2 = parse(&formatted).unwrap();
        assert_eq!(parsed, parsed2);
    }
}
