# hardy-bpv7 Design

Bundle Protocol version 7 implementation per [RFC 9171](https://www.rfc-editor.org/rfc/rfc9171.html).

## Design Goals

- **Zero-copy parsing.** Bundle data is parsed in place, with structures holding byte ranges into the source buffer rather than copied data. This enables efficient payload access and CRC computation without duplication.

- **Deterministic encoding.** The library always produces deterministic CBOR output per RFC 8949 §4.2.1 (sometimes called "canonical" encoding), but reports whether the input required transformation. Callers can implement their own policy (accept, reject, log) based on this information.

- **Type-safe EID representation.** Endpoint identifiers are represented as distinct enum variants reflecting their semantic differences, not just string parsing. `LocalNode` is a separate variant because [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html) defines it as a distinct concept.

- **Separation of mechanism and policy.** The library performs parsing, validation, and transformation but doesn't impose policies. Key providers, canonicalization responses, and block handling are all caller decisions.

- **`no_std` compatibility.** The core library works on embedded platforms with only a heap allocator, though some features (system clock, serde) require `std`.

## Parsing Modes and Trust Boundaries

The library provides three parsing modes reflecting different trust levels and processing requirements.

**RewrittenBundle** is for untrusted input arriving from Convergence Layer Adaptors. It performs full processing: canonicalization, BPSec decryption and verification, and removal of unrecognised blocks per RFC 9171. Even when parsing fails, it attempts to extract enough bundle metadata to generate a status report back to the source. This mode returns a three-variant enum: `Valid` (no changes needed), `Rewritten` (canonical output differs from input), or `Invalid` (bundle unusable, but metadata available for status reporting).

**CheckedBundle** is for semi-trusted input from local application services. These bundles shouldn't contain invalid blocks (the local service created them), but may need canonicalization. This mode validates and canonicalizes but doesn't remove blocks.

**ParsedBundle** is for quick inspection without modification. It parses the bundle and reports whether canonicalization would be needed, but doesn't transform anything. Useful for routing decisions or when the bundle will be forwarded unchanged.

The separation ensures that untrusted network input receives full scrutiny while locally-originated bundles avoid unnecessary processing.

## Zero-Copy Architecture

Bundles are parsed in place with structures holding `Range<usize>` values pointing into the source byte array. A `Block` doesn't contain its payload - it contains the byte range where the payload lives.

This design serves several purposes. First, large payloads aren't copied during parsing. Second, CRC validation can hash the exact byte ranges without reassembly. Third, and importantly, Range values are "recipes" rather than views - they describe where data lives without requiring it to be in memory. This creates future capability for lazy loading where only portions of a bundle are fetched from storage as needed.

When blocks are encrypted by a BCB, the decrypted content must be stored somewhere. The `Payload` enum handles this with two variants: `Borrowed` (a reference into the original buffer) and `Decrypted` (owned data that's automatically zeroed when dropped). This maintains the zero-copy model for unencrypted blocks while properly handling decrypted content.

## Builder and Editor

The library provides two patterns for bundle construction.

**Builder** is a factory for creating new bundles from scratch. It uses a fluent API where callers specify source, destination, lifetime, flags, and payload, then call `build()` to produce a complete bundle and its CBOR encoding.

**Editor** is for modifying existing bundles. It's optimised for the forwarding case where most of the bundle stays the same. Rather than re-encoding everything, Editor tracks what changed and surgically updates only the affected portions. For a forwarding node adding Previous Node and incrementing Hop Count, this avoids re-encoding the entire payload.

The distinction matters for performance. A node forwarding thousands of bundles per second benefits significantly from patching two fields versus re-encoding megabytes of payload data.

## BPSec Integration

The library implements RFC 9172 (Bundle Protocol Security) with a pluggable architecture for security context providers.

### Pluggable Security Contexts

Security processing is built around the concept of pluggable security context providers. When processing BIBs and BCBs, the library calls out to registered providers that implement the actual cryptographic operations. This separation allows:

- Different deployments to use different key management systems
- Security contexts to be added without modifying the core library
- Feature-flagged inclusion of specific contexts (e.g., RFC 9173 default contexts)

The library exposes a common key representation based on JWK (JSON Web Key) format, providing flexible data types for managing keychains.

### Security Block Structure

Following RFC 9172's design, security operations within a BIB or BCB share context parameters but produce unique results per target. The library reflects this by using reference-counted parameters shared across operations while maintaining separate result storage for each target block. This matches the wire format where a single security block can protect multiple targets with one set of parameters but distinct cryptographic results.

### Processing Order

Bundle security processing follows a specific order to give key providers maximum information for their decisions.

First, all blocks are parsed and BCB targets are marked. At this point, the key provider knows the bundle structure and which blocks are encrypted, but hasn't been asked for decryption keys yet.

Second, Block Integrity Blocks (BIBs) are verified. Now the key provider can see which blocks have integrity protection.

Third, remaining encrypted blocks are decrypted. The key provider now has full visibility into the bundle's decrypted content.

This progressive disclosure supports sophisticated key policies. A key provider might release certain keys only if the bundle has integrity protection, or select different keys based on decrypted header content. The library doesn't impose a key policy - it just ensures maximum information is available at each decision point.

### Future Work: COSE Security Contexts

The pluggable security context architecture is designed to support additional security contexts as they reach standardisation. The BPSec COSE context (draft-ietf-dtn-bpsec-cose) is a stretch goal that will be integrated once the specification stabilises, following the same pattern as the RFC 9173 contexts.

## Endpoint Identifier Design

EIDs are represented as a type-safe enum rather than parsed strings. The variants reflect semantic differences defined in the specifications.

`LocalNode` represents `ipn:!.<service>` as defined in [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html). This is explicitly a separate variant because it's semantically distinct from "a remote node that happens to be us." Pattern matching forces code to handle the local case explicitly, and the compiler prevents accidentally routing to yourself via the network.

`Ipn` and `LegacyIpn` both represent three-component IPN addresses per RFC 9758 (Allocator, Node, Service), but preserve which CBOR wire format was used during decode:

- `Ipn` indicates the three-element CBOR encoding (RFC 9758 §6.1.2) where Allocator, Node, and Service are separate array elements
- `LegacyIpn` indicates the two-element CBOR encoding (RFC 9171 / RFC 9758 §6.1.1) where Allocator and Node are packed into a single 64-bit FQNN

This distinction is primarily important for interoperability. When communicating with implementations that do not support three-element encoding (those predating RFC 9758), responses must use two-element encoding. Preserving the decode format allows the BPA to make this decision.

On encode, `Ipn` with allocator_id=0 emits two-element encoding for efficiency per RFC 9758's recommendation, while non-zero allocators require three-element encoding. `LegacyIpn` always emits two-element encoding.

The underlying hardy-cbor library is agnostic to this distinction - it simply handles CBOR array encoding and decoding without IPN-specific knowledge.

`Dtn` handles the `dtn://` URI scheme. `Unknown` captures future schemes the library doesn't recognise.

The separate `NodeId` and `Service` types reflect the routing distinction: nodes route bundles based on `NodeId`, while services receive bundles based on `Service`. This separation is useful for routing table lookups and service registration.

## Deterministic Encoding Strategy

RFC 9171 requires bundles to conform to the core deterministic encoding requirements of RFC 8949 §4.2.1 (sometimes referred to as "canonical" encoding). The library's approach is:

1. Always produce deterministic output
2. Report whether transformation was needed
3. Let the caller decide the policy response

This separation means the library handles the mechanical transformation while applications implement their own policies. A strict deployment might reject non-conformant input. A permissive deployment might accept and log. A monitoring system might track non-conformant sources for analysis.

The `shortest` flag from hardy-cbor propagates through parsing, and the library tracks whether any block required rewriting. For `RewrittenBundle`, the distinction between `Valid` and `Rewritten` variants tells the caller exactly what happened.

## Block Handling

Extension blocks use the same zero-copy approach as the primary block. Each `Block` structure contains its type, flags, CRC information, and byte ranges into the source data.

The `BibCoverage` enum tracks integrity protection state: `None` (no BIB covers this block), `Some(block_number)` (protected by a specific BIB), or `Maybe` (encrypted BIBs couldn't be decrypted, so coverage is unknown). This three-state model allows code to distinguish between "definitely unprotected" and "unknown protection status."

Unrecognised blocks are handled per RFC 9171 rules. In `RewrittenBundle` mode, blocks marked with "delete on failure" flags are removed. The library rewrites BIBs and BCBs that targeted removed blocks, potentially removing security blocks entirely if all their targets are gone.

## Utility Types

The library provides type-safe representations of the various fields and structures defined in RFC 9171. These types enforce correctness through the Rust type system rather than relying on runtime validation of raw values.

Key utility types include `CreationTimestamp` (handling both clocked and clockless bundle creation), `DtnTime` (DTN epoch-based timestamps with conversion helpers), `HopInfo` (hop limit and count), `BundleAge` (elapsed time since creation for clockless nodes), `FragmentInfo` (ADU fragmentation), and `bundle::Id` (unique bundle identification with serialization support for database keying). Flag fields from the primary block and extension blocks are represented as type-safe structures rather than raw bitmasks. The `status_report` module provides `BundleStatusReport` and related types for generating and parsing RFC 9171 administrative records, including the full set of reason codes.

These types implement the `ToCbor` and `FromCbor` traits for wire format encoding, and optionally `serde` traits for metadata persistence. Full API documentation is available via rustdoc.

## Integration

### With hardy-cbor

The library builds on hardy-cbor's wire-format parsing. It uses closure-based array parsing for structural integrity, Range returns for zero-copy access, and `shortest` flag propagation for deterministic encoding detection. The separation is deliberate: hardy-cbor handles CBOR-level concerns while hardy-bpv7 handles bundle-level semantics.

### With hardy-bpa

The Bundle Processing Agent uses all three parsing modes depending on bundle origin. CLA input uses `RewrittenBundle` for full validation. Service input uses `CheckedBundle` for canonicalization without block removal. Quick routing decisions might use `ParsedBundle` for inspection without transformation.

### CLI Tools

The `hardy-bpv7-tools` package provides a `bundle` command-line utility for creating, inspecting, and manipulating bundles. It exercises the full library API including Builder, Editor, and BPSec operations, and serves as both a practical tool and a reference implementation. See [tools/docs/design.md](../tools/docs/design.md) for details.

## Serialization Support

Beyond CBOR wire format encoding, the library supports serialization of internal bundle structures for storage and debugging purposes.

The `serde` feature enables generic serialization via the serde framework, allowing bundle metadata to be persisted in formats like JSON. This is useful for metadata storage implementations that need to index bundle information without storing the full CBOR encoding.

This feature requires `std` and is disabled by default to maintain `no_std` compatibility for the core library.

## Dependencies

The library is `no_std` compatible, suitable for embedded platforms with only a heap allocator.

Feature flags control optional functionality:

- **`std`**: Enables system clock access for `CreationTimestamp` generation and propagates `std` to cryptographic dependencies. Without this feature, timestamps follow RFC 9171's "no accurate clock" behaviour.
- **`serde`**: Enables serde-based serialization of bundle structures. Requires `std`.
- **`rfc9173`**: Enables RFC 9173 default security contexts (HMAC-SHA and AES-GCM variants). Enables `bpsec`.
- **`bpsec`** (internal): Enables the `signer` and `encryptor` modules for BPSec operations. Automatically enabled by security context features (`rfc9173`, and future `cose`). Not intended for direct use.

### Embedded Targets and Custom RNG

The `rfc9173` feature requires random number generation for cryptographic operations (key generation, nonces). This is provided by the `getrandom` crate, which uses OS-provided entropy by default.

For embedded targets without OS RNG support, you must provide a custom entropy source using getrandom's [custom backend](https://docs.rs/getrandom/latest/getrandom/#custom-backend):

1. Enable the `custom` feature on getrandom in your Cargo.toml:
   ```toml
   [dependencies]
   getrandom = { version = "0.3", features = ["custom"] }
   ```

2. Implement the custom backend function:
   ```rust
   use getrandom::Error;

   #[unsafe(no_mangle)]
   unsafe extern "Rust" fn __getrandom_v03_custom(
       dest: *mut u8,
       len: usize,
   ) -> Result<(), Error> {
       // Fill dest with entropy from your hardware RNG, TRNG, etc.
       todo!()
   }
   ```

See [getrandom's documentation](https://docs.rs/getrandom/latest/getrandom/) for the full list of supported targets and custom backend details.

## Standards Compliance

- [RFC 9171](https://www.rfc-editor.org/rfc/rfc9171.html) - Bundle Protocol Version 7
- [RFC 9172](https://www.rfc-editor.org/rfc/rfc9172.html) - Bundle Protocol Security (BPSec)
- [RFC 9173](https://www.rfc-editor.org/rfc/rfc9173.html) - Default Security Contexts (behind feature flag)
- [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html) - Three-component IPN scheme

## Testing

- [Unit Test Plan](unit_test_plan.md) - RFC 9171 parsing, factories, EID logic
- [Fuzz Test Plan](fuzz_test_plan.md) - Bundle parsing, EID string/CBOR parsing
- [Component Test Plan](component_test_plan.md) - CLI-driven verification of library logic
- [BPSec Unit Test Plan](../src/bpsec/unit_test_plan.md) - RFC 9172/3 Integrity & Confidentiality
