# hardy-cbor Design

CBOR encoding/decoding optimised for BPv7 wire format requirements.

## Design Goals

- **Wire format, not document format.** BPv7 uses CBOR as a schema-fixed binary wire format. At every point in the byte stream, the parser knows exactly what structure to expect. This is fundamentally different from general-purpose document parsing where the schema might vary.

- **Zero-copy access.** Rather than copying byte slices out of the source buffer, parsing returns byte ranges that reference the original data. This enables efficient access to block payloads and allows CRC computation over specific byte spans without duplication.

- **Structural integrity.** When encoding or decoding a sequence (array or map), the library must verify that element counts and terminators are correct. There should be no possibility of smuggled bytes between elements or missing entries going undetected.

- **Shortest-form tracking.** RFC 9171 requires bundles to conform to the core deterministic encoding requirements of RFC 8949 §4.2.1 (sometimes referred to as "canonical encoding"). This includes using the minimum number of bytes for each value. The library must detect whether parsed values used shortest-form encoding so that higher layers can enforce this policy.

- **`no_std` compatibility.** The library must be usable on embedded platforms with only a heap allocator available.

## Why Not Existing Crates?

Before implementing a custom CBOR library, we evaluated the existing Rust ecosystem, primarily `serde_cbor` and `ciborium`.

The serde-based crates are designed for flexible document parsing across multiple formats (JSON, YAML, CBOR). This flexibility comes with overhead - runtime type inspection and intermediate representations - that provides no benefit when the schema is fixed by RFC. BPv7 always knows exactly what structure to expect at each point in the byte stream.

More fundamentally, the existing crates parse permissively. They don't report whether input used shortest-form encoding, which is required for RFC 9171's deterministic encoding requirements. They also copy data out rather than returning byte ranges into the source buffer, preventing the zero-copy access needed for efficient block payload handling and CRC computation.

Finally, there were maintenance concerns: `serde_cbor` has been criticised for performance issues and is largely unsupported, while `ciborium` was unmaintained at the time of evaluation and had an awkward API.

Given these limitations, a purpose-built library provides the control and performance needed for a wire-format parser.

## Key Design Decisions

### Closure-Based Parsing

Arrays and maps are parsed using closures (callback functions) rather than iterators:

```rust
parse_array(data, |array, shortest, tags| {
    let x: i32 = array.parse()?;
    let y: i32 = array.parse()?;
    Ok((x, y))
})
```

The closure (the `|array, shortest, tags| { ... }` block) is called by `parse_array` with a parsing context, and must return a result. This is similar to passing a function pointer in C, but with the ability to capture variables from the surrounding scope.

The closure pattern creates a parsing scope with well-defined entry and exit points. On entry, the library sets up the array context. On exit, it verifies the sequence ended correctly - either the definite count was matched, or the indefinite-length break code was found. This prevents several classes of parsing errors: consuming fewer items than declared (leaving bytes that corrupt subsequent parsing), consuming more items than exist (reading into unrelated data), or silently accepting malformed terminators.

An iterator-based approach would allow callers to `break` early or simply drop the iterator, leaving the byte stream position in an undefined state. The closure pattern makes correct usage the path of least resistance.

### Shortest-Form Tracking

Decoding returns a tuple of `(value, shortest, len)` where `shortest` indicates whether the value used minimum-length encoding.

The flag is deliberately called `shortest` rather than `deterministic` or `canonical` because RFC 8949 §4.2 defines deterministic encoding with multiple rules: shortest encoding, map key ordering, and definite-length preference. (The term "canonical" was used in the earlier RFC 7049 and remains common in code and documentation.) This library only tracks the shortest-encoding property at the individual value level. Full deterministic encoding depends on higher-level structure, which is hardy-bpv7's responsibility.

Additionally, BPv7 has its own conformance rules that don't always align with RFC 8949's full deterministic encoding. Keeping the terminology precise at each layer avoids confusion about what guarantees are actually being provided.

### Zero-Copy via Range Returns

Rather than copying data out of the source buffer, the library returns byte ranges that reference the original data.

When decoding, `FromCbor` returns the number of bytes consumed, allowing callers to track their position in the buffer. When parsing byte strings, `Value::Bytes` returns a `Range<usize>` into the source data rather than a copied slice. When encoding, `ToCbor::Result` can return a `Range<usize>` indicating where the encoded data landed in the output buffer.

This design supports several use cases in hardy-bpv7. Block payloads can reference ranges in the bundle's byte array without copying the data. CRC computation can hash specific byte ranges while excluding the CRC field itself. BPSec operations can identify the exact byte extents that signatures cover.

### Series\<D\> Const Generic

The library uses a single `Series<D>` type to handle array, map, and bare sequence parsing, where `D` is a compile-time constant (similar to C++ template integer parameters).

The three cases are: `D=1` for arrays (one CBOR item per logical element), `D=2` for maps (two items per element: key and value), and `D=0` for bare sequences of concatenated CBOR items with no container.

The iteration logic is structurally identical across all three cases - only the number of items per logical step differs. Using a compile-time constant rather than a runtime value allows the compiler to generate optimised code for each variant. The `D=0` case exists specifically because BPSec defines structures as concatenations of CBOR values without an enclosing array.

It's important to note that a bare sequence (`D=0`) is fundamentally different from an indefinite-length array. An indefinite-length array has a `0x9F` header and `0xFF` break terminator; the library knows when it ends. A bare sequence has no framing at all - `at_end()` simply checks whether all input bytes have been consumed.

### Pull-Parser Support

When parsing encounters incomplete input, it returns `Error::NeedMoreData(n)` indicating how many more bytes are required before parsing can succeed.

This enables streaming scenarios where callers accumulate bytes from a network socket or other source, attempt to parse, and retry when more data arrives. The feature was added in response to a user request and was trivial to implement given the library's architecture. It is not currently used within Hardy itself, but provides flexibility for external consumers.

### Tag Handling

CBOR supports semantic tags that provide additional type information about encoded values. The library provides explicit tag handling on both the encoding and decoding sides.

For decoding, every parsing callback receives a `tags` parameter containing any semantic tags that preceded the value. This allows callers to detect and handle tagged values according to their policy. Importantly, for BPv7 purposes, the presence of any tag causes the `shortest` flag to be reported as `false` - reflecting that RFC 9171 does not use tagged values in its wire format, except in obscure corner cases.

For encoding, two wrapper types are provided:

- `Tagged<const TAG: u64, T>` encodes a value with a compile-time constant tag number. This is efficient when the tag is known statically, such as tag 24 for embedded CBOR.
- `RuntimeTagged<T>` encodes a value with a runtime-determined tag number. This supports scenarios where tag numbers are computed dynamically, such as when re-encoding parsed data.

Both wrappers can be nested to apply multiple tags to a single value.

## Integration

### With hardy-bpv7

The hardy-bpv7 library is the primary consumer of hardy-cbor. It uses the CBOR primitives for bundle and block parsing with byte-range tracking, shortest-form detection for RFC 9171 compliance, CRC computation over specific byte extents, and zero-copy payload access.

The separation between the two libraries is deliberate: hardy-cbor handles CBOR-level concerns (encoding, decoding, shortest-form detection), while hardy-bpv7 handles BPv7-level concerns (bundle structure, block semantics, deterministic encoding policy).

### Trait-Based Encoding/Decoding

Types implement `ToCbor` and `FromCbor` traits rather than serde's `Serialize` and `Deserialize`. Traits are Rust's equivalent of Java interfaces or C++ abstract base classes - they define a contract that implementing types must fulfil.

Defining custom traits rather than using serde provides several benefits. The `FromCbor` trait can return `(value, shortest, len)` tuples, which serde's trait signatures don't allow. The `ToCbor` trait can track byte positions through an associated result type. And encoding goes directly to CBOR bytes without passing through serde's intermediate representation.

## Standards Compliance

The library implements [RFC 8949](https://www.rfc-editor.org/rfc/rfc8949.html) CBOR encoding, with particular attention to [§4.2.1](https://www.rfc-editor.org/rfc/rfc8949.html#section-4.2.1) preferred serialization (shortest form).

On encode, the library always produces shortest-form output. On decode, it reports whether the input used shortest-form encoding. Full RFC 8949 §4.2 deterministic encoding - which includes additional rules like map key ordering - is not enforced at this layer; that responsibility belongs to higher-level libraries like hardy-bpv7.

## Dependencies

The library is marked `no_std` and has minimal dependencies:

- `thiserror` for error type derivation
- `half` for IEEE 754 half-precision (16-bit) floating point support
- `num-traits` for generic numeric operations

The half-precision float support is included for CBOR specification completeness, though it is not used by RFC 9171, RFC 9172, RFC 9173, or RFC 9174. BPv7 wire formats do not employ floating point values.

## Testing

- [Unit Test Plan](unit_test_plan.md) - RFC 8949 compliance, encode/decode round-trips
- [Fuzz Test Plan](fuzz_test_plan.md) - Decoder robustness against malformed input
