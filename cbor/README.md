# hardy-cbor

A canonical CBOR encoder and decoder implementing [RFC 8949](https://www.rfc-editor.org/rfc/rfc8949.html).

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

hardy-cbor provides `no_std`-compatible CBOR encoding and decoding with a focus on canonical (deterministic) output. It is the serialisation layer used by hardy-bpv7 for bundle wire format and by hardy-tvr for contact plan serialisation.

The crate offers two APIs: a trait-based approach (`ToCbor` / `FromCbor`) for direct struct conversion, and a streaming API (`parse_value`, `parse_array`, `parse_map`, `Encoder`) for zero-copy, callback-driven processing.

## Features

- Canonical encoding -- values always serialise to the shortest possible representation
- `no_std` with `alloc` -- no standard library dependency
- Semantic tag support via const-generic `Tagged<N, T>` wrapper
- Indefinite-length arrays and maps
- Canonical form detection -- the decoder reports whether input uses shortest encoding
- Incomplete item detection -- distinguishes truncated input from malformed data
- Opportunistic parsing -- inspect values without allocating (byte strings returned as ranges)
- Half-precision (f16) float support via the `half` crate

## Usage

### Encoding

```rust
use hardy_cbor::encode::{self, Encoder, ToCbor};

struct Point { x: i32, y: i32 }

impl ToCbor for Point {
    type Result = ();
    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit_array(Some(2), |a| {
            a.emit(&self.x);
            a.emit(&self.y);
        });
    }
}

let (bytes, _) = encode::emit(&Point { x: 10, y: -20 });
assert_eq!(bytes, &[0x82, 0x0A, 0x33]);
```

### Decoding

```rust
use hardy_cbor::decode::{self, Value};

let bytes = &[0xd8, 0x18, 0x45, 0x68, 0x65, 0x6c, 0x6c, 0x6f];

let ((), len) = decode::parse_value(bytes, |value, shortest, tags| {
    assert_eq!(tags, &[24]);
    assert!(matches!(value, Value::Bytes(range) if &bytes[range.clone()] == b"hello"));
    Ok::<_, decode::Error>(())
}).unwrap();
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [API Documentation](https://docs.rs/hardy-cbor)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
