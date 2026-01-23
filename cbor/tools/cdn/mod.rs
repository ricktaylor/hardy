/*!
CBOR Diagnostic Notation (CDN) support

This module provides types and functions for working with CBOR Diagnostic Notation,
a human-readable text representation of CBOR data defined in RFC 8949 §8.

# Features

- **Lossless**: Preserves all CBOR semantics (tags, types, indefinite-length containers)
- **Round-tripping**: CBOR ↔ CDN conversion without information loss
- **Human-readable**: Easy to read, write, and edit by hand

# Examples

```rust
use hardy_cbor_tools::cdn::{CdnValue, parse, format_cbor};
use hardy_cbor::encode;

// Create CDN value
let value = CdnValue::Array(vec![
    CdnValue::Unsigned(1),
    CdnValue::TextString("hello".to_string()),
]);

// Convert to CBOR
let cbor_bytes = encode::emit(&value).0;

// Convert back to CDN text
let cdn_text = format_cbor(&cbor_bytes, false).unwrap();
// Output: [1, "hello"]

// Parse CDN text
let parsed = parse(&cdn_text).unwrap();
assert_eq!(parsed, value);
```
*/

pub mod ast;
pub mod formatter;
pub mod parser;

// Re-export main types for convenience
#[allow(unused_imports)]
pub use ast::CdnValue;
pub use formatter::format_cbor;
pub use parser::parse;
