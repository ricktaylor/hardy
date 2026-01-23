/*!
CDN parser using Chumsky

Parses CBOR Diagnostic Notation (CDN) text into AST.
*/

use super::ast::CdnValue;
use base64::prelude::*;
use chumsky::prelude::*;

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
pub fn parse(input: &str) -> Result<CdnValue, Vec<Simple<char>>> {
    cdn_parser().parse(input)
}

/// Build the complete CDN parser
fn cdn_parser() -> impl Parser<char, CdnValue, Error = Simple<char>> {
    value_parser().then_ignore(end())
}

/// Parse a single CDN value (recursive)
fn value_parser() -> impl Parser<char, CdnValue, Error = Simple<char>> {
    recursive(|value| {
        let whitespace = || filter(|c: &char| c.is_whitespace()).repeated();

        // Unsigned integer: 0, 42, 1000000
        let unsigned = text::int(10)
            .try_map(|s: String, span| {
                s.parse::<u64>()
                    .map(CdnValue::Unsigned)
                    .map_err(|e| Simple::custom(span, format!("Invalid unsigned integer: {}", e)))
            })
            .labelled("unsigned integer");

        // Negative integer: -1, -42, -1000000
        let negative = just('-')
            .ignore_then(text::int(10))
            .try_map(|s: String, span| {
                s.parse::<i64>()
                    .map(|n| CdnValue::Negative(-n))
                    .map_err(|e| Simple::custom(span, format!("Invalid negative integer: {}", e)))
            })
            .labelled("negative integer");

        // Float: 1.5, -3.14159, 1.0e10
        let float = {
            let sign = just('-').or_not();
            let integer = text::int(10);
            let fraction = just('.').chain(text::digits(10));
            let exponent = one_of("eE")
                .chain(one_of("+-").or_not())
                .chain(text::int(10));

            sign.chain::<char, _, _>(integer)
                .chain::<char, _, _>(
                    fraction
                        .or(exponent.clone())
                        .repeated()
                        .at_least(1)
                        .flatten(),
                )
                .collect::<String>()
                .try_map(|s, span| {
                    s.parse::<f64>()
                        .map(CdnValue::Float)
                        .map_err(|e| Simple::custom(span, format!("Invalid float: {}", e)))
                })
                .labelled("float")
        };

        // Hex byte string: h'deadbeef'
        let hex_bytes = just("h'")
            .ignore_then(filter(|c: &char| c.is_ascii_hexdigit()).repeated())
            .then_ignore(just('\''))
            .try_map(|chars, span| {
                let hex_str: String = chars.iter().collect();
                hex::decode(&hex_str)
                    .map(CdnValue::ByteString)
                    .map_err(|e| Simple::custom(span, format!("Invalid hex string: {}", e)))
            })
            .labelled("hex byte string");

        // Base64 byte string: b64'SGVsbG8='
        let b64_bytes = just("b64'")
            .ignore_then(filter(|c: &char| *c != '\'').repeated())
            .then_ignore(just('\''))
            .try_map(|chars, span| {
                let b64_str: String = chars.iter().collect();
                BASE64_STANDARD
                    .decode(&b64_str)
                    .map(CdnValue::ByteString)
                    .map_err(|e| Simple::custom(span, format!("Invalid base64 string: {}", e)))
            })
            .labelled("base64 byte string");

        // Text string: "hello world"
        // Parse string content - handle escapes by accepting backslash followed by any char
        let escape = just('\\').then(any()).map(|(_, c)| vec!['\\', c]);
        let normal = none_of("\"\\").map(|c| vec![c]);

        let text_string = just('"')
            .ignore_then(
                escape
                    .or(normal)
                    .repeated()
                    .map(|vecs: Vec<Vec<char>>| vecs.into_iter().flatten().collect::<String>()),
            )
            .then_ignore(just('"'))
            .map(|s: String| CdnValue::TextString(unescape_string(&s)))
            .labelled("text string");

        // Array: [1, 2, 3] or [_ 1, 2, 3]
        let array = just('[')
            .padded_by(whitespace())
            .ignore_then(just('_').padded_by(whitespace()).or_not())
            .then(
                value
                    .clone()
                    .separated_by(just(',').padded_by(whitespace()))
                    .allow_trailing()
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
            .labelled("array");

        // Map: {1: "a", 2: "b"} or {_ 1: "a"}
        let map_entry = value
            .clone()
            .then_ignore(just(':').padded_by(whitespace()))
            .then(value.clone());

        let map = just('{')
            .padded_by(whitespace())
            .ignore_then(just('_').padded_by(whitespace()).or_not())
            .then(
                map_entry
                    .separated_by(just(',').padded_by(whitespace()))
                    .allow_trailing()
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
            .labelled("map");

        // Tagged value: 24(h'...')
        let tagged = text::int(10)
            .try_map(|s: String, span| {
                s.parse::<u64>()
                    .map_err(|e| Simple::custom(span, format!("Invalid tag number: {}", e)))
            })
            .then_ignore(just('(').padded_by(whitespace()))
            .then(value.clone())
            .then_ignore(just(')').padded_by(whitespace()))
            .map(|(tag, val)| CdnValue::Tagged(tag, Box::new(val)))
            .labelled("tagged value");

        // Keywords
        let bool_true = text::keyword("true")
            .to(CdnValue::Bool(true))
            .labelled("true");

        let bool_false = text::keyword("false")
            .to(CdnValue::Bool(false))
            .labelled("false");

        let null = text::keyword("null").to(CdnValue::Null).labelled("null");

        let undefined = text::keyword("undefined")
            .to(CdnValue::Undefined)
            .labelled("undefined");

        // Simple value: simple(22)
        let simple = text::keyword("simple")
            .ignore_then(just('(').padded_by(whitespace()))
            .ignore_then(text::int(10))
            .then_ignore(just(')').padded_by(whitespace()))
            .try_map(|s: String, span| {
                s.parse::<u8>()
                    .map(CdnValue::Simple)
                    .map_err(|e| Simple::custom(span, format!("Invalid simple value: {}", e)))
            })
            .labelled("simple value");

        // Combine all value parsers
        // Order matters: try more specific patterns first
        choice((
            tagged,     // Must be before unsigned (to catch "24()")
            bool_true,  // Before unsigned
            bool_false, // Before unsigned
            null,       // Before unsigned
            undefined,  // Before unsigned
            simple,     // Before unsigned
            float,      // Before negative and unsigned
            negative,   // Before unsigned (to catch the "-" sign)
            unsigned,
            hex_bytes,
            b64_bytes,
            text_string,
            array,
            map,
        ))
        .padded_by(whitespace())
    })
}

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
                _ => result.push(c),
            }
        } else {
            result.push(c);
        }
    }

    result
}

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
