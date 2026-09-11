# hardy-bpv7 Design

Bundle Protocol version 7 implementation per [RFC 9171](https://www.rfc-editor.org/rfc/rfc9171.html).

## Design Goals

- **Zero-copy parsing.** Bundle data is parsed in place, with structures holding byte ranges into the source buffer rather than copied data. This enables efficient payload access and CRC computation without duplication.

- **Deterministic encoding.** The library always produces deterministic CBOR output per RFC 8949 §4.2.1 (sometimes called "canonical" encoding), and requires it on input: a non-shortest encoding is rejected as `NotCanonical` rather than silently normalised.

- **Type-safe EID representation.** Endpoint identifiers are represented as distinct enum variants reflecting their semantic differences, not just string parsing. `LocalNode` is a separate variant because [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html) defines it as a distinct concept.

- **Separation of mechanism and policy.** The library performs parsing, validation, and transformation but doesn't impose policies. Key sources, security-failure dispositions, and unrecognised-block handling are all caller decisions.

- **`no_std` compatibility.** The core library works on embedded platforms with only a heap allocator, though some features (system clock, serde) require `std`.

## Parsing and Validation Layers

There is one entry point, not a menu of modes. `parser::parse` (also re-exported as `hardy_bpv7::parse`) performs the keyless structural parse and returns a `Parsed`, holding the authoritative byte buffer, the `Bundle` (primary block plus the blocks map), and the decoded BIB and BCB OperationSets. Everything keyed is layered on top of that by composing the primitives in the `checks` module.

The trust level is therefore the caller's choice of which layers to compose, not a parser argument. A CLA ingress path runs the structural parse, then `checks::verify` (the composed §B → §C8 → §C7 keyed pass), then `checks::apply_rewrites` to emit the corrected wire bytes. A path that only needs to inspect a bundle stops after `parse`. A path that trusts its source but still needs integrity facts can run `verify` and ignore the rewrite step.

This is the mechanism/policy split made concrete: `checks` produces facts (decrypted / NoKey / decrypt-failed, block coverage, unsupported-block classification) and never decides accept-or-reject. Which facts justify dropping a block, dropping a bundle, or raising a status report is the consumer's business, and differs between the BPA ingress path, the CLI tools, and the fuzz harness. The full pipeline, including the `§A`–`§E` section labels the code carries, is documented in [parser_design.md](parser_design.md).

## Zero-Copy Architecture

Bundles are parsed in place with structures holding `Range` values pointing into the source byte array. A `Block` doesn't contain its payload - it contains the byte range where the payload lives.

This design serves several purposes. First, large payloads aren't copied during parsing. Second, CRC validation can hash the exact byte ranges without reassembly. Third, and importantly, Range values are "recipes" rather than views - they describe where data lives without requiring it to be in memory. This creates future capability for lazy loading where only portions of a bundle are fetched from storage as needed.

`Block::extent` and `Block::data` are `Range<u64>`, not `Range<usize>`, because the offset domain is the wire stream rather than an in-memory buffer: CBOR offsets are `u64`, a streamed bundle need not fit in `usize` on a 32-bit target, and a `Range` that is a recipe for a future storage read has no reason to be bounded by the current address space. Callers holding the whole bundle in memory narrow to `usize` at the slice point. The builder and editor still work in `usize` internally; converting them is tracked in [TODO.md](TODO.md).

When blocks are encrypted by a BCB, the decrypted content must be stored somewhere. The `Payload` enum handles this with two variants: `Borrowed` (a reference into the original buffer) and `Decrypted` (owned data that's automatically zeroed when dropped). This maintains the zero-copy model for unencrypted blocks while properly handling decrypted content.

## Builder and Editor

The library provides two patterns for bundle construction.

**Builder** is a factory for creating new bundles from scratch. It uses a fluent API where callers specify source, destination, lifetime, flags, and payload, then call `build()` to produce a complete bundle and its CBOR encoding.

**Editor** is for modifying existing bundles. It's optimised for the forwarding case where most of the bundle stays the same. Rather than re-encoding everything, Editor tracks what changed and surgically updates only the affected portions. For a forwarding node adding Previous Node and incrementing Hop Count, this avoids re-encoding the entire payload.

The distinction matters for performance. A node forwarding thousands of bundles per second benefits significantly from patching two fields versus re-encoding megabytes of payload data.

## BPSec Integration

The library implements RFC 9172 (Bundle Protocol Security). Key material is pluggable; the set of security contexts is not.

### Closed, Feature-Gated Security Contexts

Each security context is a module under `bpsec::context` (`bib_hmac_sha2`, `bcb_aes_gcm`), and dispatch to it happens through the `Context` enums in `bpsec::signer` and `bpsec::encryptor`, whose variants are gated on the corresponding cargo feature (`rfc9173` today). Adding a context means adding a variant and a module, both behind the new feature.

This is a deliberate choice against a `SecurityContext` trait with registered implementations:

- **Exhaustiveness is the point.** A new context has to be threaded through parameter decoding, results decoding, the signer and encryptor entry points, and the edit cascade. With a closed enum the compiler enumerates every one of those seams; with a trait they become runtime lookups that compile fine while silently doing nothing.
- **The parameters are heterogeneous.** BIB-HMAC-SHA2 and BCB-AES-GCM have genuinely different parameter and result shapes. A trait would have to type-erase them and hand each implementation an opaque bag to downcast, which trades a compile-time check for a runtime one and buys nothing.
- **There is no second implementor.** The extension point exists for contexts that reach standardisation, not for deployments. A trait boundary designed for one hypothetical out-of-tree implementor is cost without a user.

What *is* pluggable is key material: callers supply a `bpsec::key::KeySource`, so different deployments can use different key management without touching the crate. The key representation is based on the JWK (JSON Web Key) format, giving flexible data types for managing keychains.

### Security Block Structure

Following RFC 9172's design, security operations within a BIB or BCB share context parameters but produce unique results per target. The library reflects this by using reference-counted parameters shared across operations while maintaining separate result storage for each target block. This matches the wire format where a single security block can protect multiple targets with one set of parameters but distinct cryptographic results.

### Processing Order

The keyed pass runs decryption before verification, in the order §B → §C8 → §C7, and `checks::verify` composes the three steps so that call sites cannot get the order wrong.

**§B: decrypt and validate BCB-covered BIBs.** The keyless structural parse cannot read the target list of a BCB-encrypted BIB, so it conservatively marks every block that BIB might cover as `BibCoverage::Maybe`. §B decrypts those BIBs and replaces the guesses with the real coverage; §B6 collapses any residual `Maybe` to `None` once every encrypted BIB is accounted for.

**§C8: decrypt BCB-protected extension blocks.** `PreviousNode`, `BundleAge`, and `HopCount` bodies are recovered here, or recorded as NoKey / decrypt-failed.

**§C7: verify every BIB.** Verification reads plaintext recovered by the two preceding steps: a BIB may cover an extension block whose body only exists after §C8, and an encrypted BIB is only readable after §B. That dependency, not a key-disclosure policy, is what fixes the order.

The steps thread one shared decrypted-plaintext map, so a block is decrypted once regardless of how many later steps read it. The library imposes no key policy: it hands the `KeySource` the block context and records the outcome, and the caller decides what a NoKey or a failed decrypt means.

### Future Work: COSE Security Contexts

The BPSec COSE context (draft-ietf-dtn-bpsec-cose) is a stretch goal, to be integrated once the specification stabilises. It lands the same way RFC 9173 did: a `bpsec::context::cose*` module, a feature-gated variant on the signer and encryptor `Context` enums, and whatever compile errors the exhaustive matches then raise.

## Endpoint Identifier Design

EIDs are represented as a type-safe enum rather than parsed strings. The variants reflect semantic differences defined in the specifications.

`LocalNode` represents `ipn:!.<service>` as defined in [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html). This is explicitly a separate variant because it's semantically distinct from "a remote node that happens to be us." Pattern matching forces code to handle the local case explicitly, and the compiler prevents accidentally routing to yourself via the network.

`Ipn` and `LegacyIpn` both represent three-component IPN addresses per RFC 9758 (Allocator, Node, Service), but preserve which CBOR wire format was used during decode:

- `Ipn` indicates the three-element CBOR encoding (RFC 9758 §6.1.2) where Allocator, Node, and Service are separate array elements
- `LegacyIpn` indicates the two-element CBOR encoding (RFC 9171 / RFC 9758 §6.1.1) where Allocator and Node are packed into a single 64-bit FQNN

This distinction is primarily important for interoperability. When communicating with implementations that do not support three-element encoding (those predating RFC 9758), responses must use two-element encoding. Preserving the decode format allows the BPA to make this decision.

On decode, two-element encoding with allocator_id=0 is normalised to `Ipn` (since two-element and three-element are semantically identical when the allocator is zero). Two-element encoding with allocator_id!=0 is preserved as `LegacyIpn`, since the encoding choice is significant -- it indicates the peer used legacy format despite having a non-zero allocator.

On encode, `Ipn` with allocator_id=0 emits two-element encoding for efficiency per RFC 9758's recommendation, while non-zero allocators require three-element encoding. `LegacyIpn` always emits two-element encoding.

The underlying hardy-cbor library is agnostic to this distinction - it simply handles CBOR array encoding and decoding without IPN-specific knowledge.

`Dtn` handles the `dtn://` URI scheme. `Unknown` captures future schemes the library doesn't recognise.

The separate `NodeId` and `Service` types reflect the routing distinction: nodes route bundles based on `NodeId`, while services receive bundles based on `Service`. This separation is useful for routing table lookups and service registration.

## Deterministic Encoding Strategy

RFC 9171 requires bundles to conform to the core deterministic encoding requirements of RFC 8949 §4.2.1 (sometimes referred to as "canonical" encoding). The library's approach is:

1. Always produce deterministic output
2. Require deterministic input, and reject anything else

The `shortest` flag from hardy-cbor propagates through parsing, and `parse_canonical` turns a non-shortest encoding into a field-labelled `NotCanonical` error rather than normalising it. Tags are rejected from their first byte, with one hand-rolled exception for the `#6.24` tag RFC 9171 permits on block data.

Rejecting rather than normalising is a security position, not a strictness preference. BPSec hashes byte ranges of the received bundle; if the parser quietly re-encoded non-canonical input, the bytes a BIB was computed over and the bytes the library later hashes could differ, and two receivers disagreeing about that transformation would disagree about integrity. Rejection keeps exactly one byte sequence corresponding to a given bundle.

What the library *does* rewrite is a separate, explicit step: `checks::apply_rewrites` re-emits or removes whole blocks (unrecognised blocks the caller elected to drop, extension fields the forwarding path updated) and returns the new wire bytes plus the updated `Bundle`. That is a deliberate caller-driven edit, not a silent normalisation during parse.

## Block Handling

Extension blocks use the same zero-copy approach as the primary block. Each `Block` structure contains its type, flags, CRC information, and byte ranges into the source data.

The `BibCoverage` enum tracks integrity protection state: `None` (no BIB covers this block), `Some(block_number)` (protected by a specific BIB), or `Maybe` (encrypted BIBs couldn't be decrypted, so coverage is unknown). This three-state model allows code to distinguish between "definitely unprotected" and "unknown protection status."

Unrecognised blocks are handled per RFC 9171 rules, in two steps that match the mechanism/policy split. `checks::classify_unsupported` (§A) reports which blocks this node cannot process and what their processing-control flags demand; a block whose flags request `delete_bundle_on_failure` is a hard error. The caller decides which of the remaining blocks to drop and passes them to `checks::apply_rewrites` (§E), which drives the BPSec-aware editor cascade: BIBs and BCBs that targeted a removed block are rewritten, and a security block whose targets are all gone is removed entirely.

A block still marked `BibCoverage::Maybe` must not be removed, because a BIB that has not yet been decrypted might depend on it. §B6 resolves the residual markers before any rewrite runs.

## Utility Types

The library provides type-safe representations of the various fields and structures defined in RFC 9171. These types enforce correctness through the Rust type system rather than relying on runtime validation of raw values.

Key utility types include `CreationTimestamp` (handling both clocked and clockless bundle creation), `DtnTime` (DTN epoch-based timestamps with conversion helpers), `HopInfo` (hop limit and count), `BundleAge` (elapsed time since creation for clockless nodes), `FragmentInfo` (ADU fragmentation), and `bundle::Id` (unique bundle identification with serialization support for database keying). Flag fields from the primary block and extension blocks are represented as type-safe structures rather than raw bitmasks. The `status_report` module provides `BundleStatusReport` and related types for generating and parsing RFC 9171 administrative records, including the full set of reason codes.

These types implement the `ToCbor` and `FromCbor` traits for wire format encoding, and optionally `serde` traits for metadata persistence. Full API documentation is available via rustdoc.

## Integration

### With hardy-cbor

The library builds on hardy-cbor's wire-format parsing. It uses closure-based array parsing for structural integrity, Range returns for zero-copy access, and `shortest` flag propagation to enforce deterministic encoding. The separation is deliberate: hardy-cbor handles CBOR-level concerns while hardy-bpv7 handles bundle-level semantics.

### With hardy-bpa

The Bundle Processing Agent composes the layers per origin in `bpa/src/bundle/parse.rs`, and supplies the policy bpv7 deliberately omits: the NoKey disposition, which BPSec facts reject a bundle, the §D extension-field decode, and the mapping from failures to status-report reason codes.

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
- **`critical-section`**: Enables the `portable-atomic` critical-section fallback for targets without native 64-bit atomics (e.g., thumbv6m). Requires a `critical-section` implementation from your HAL or runtime.
- **`bpsec`** (internal): Enables the `signer` and `encryptor` modules for BPSec operations. Automatically enabled by security context features (`rfc9173`, and future `cose`). Not intended for direct use.

### Embedded Targets and Custom RNG

The `rfc9173` feature requires random number generation for cryptographic operations (key generation, IVs). All of it goes through `rand::rngs::SysRng`, reached via the crate's own `bpsec::context::rand_bytes` / `rand_array` helpers so there is a single audited entropy path; `rand` in turn sources entropy from `getrandom` v0.4, which uses the OS by default.

For embedded targets without OS RNG support, you must supply a custom entropy source through getrandom's [custom backend](https://docs.rs/getrandom/0.4/getrandom/#custom-backend), typically via a `RUSTFLAGS` override or a platform crate dependency. See [getrandom's documentation](https://docs.rs/getrandom/0.4/getrandom/) for the supported-target list and the configuration each one needs.

### Targets Without 64-bit Atomics

Some embedded targets (e.g., thumbv6m-none-eabi) don't support native 64-bit atomic operations. The library uses [`portable-atomic`](https://docs.rs/portable-atomic) to provide `AtomicU64` on all platforms.

On targets without native 64-bit atomics, enable the `critical-section` feature and provide a critical-section implementation:

```toml
[dependencies]
hardy-bpv7 = { version = "...", features = ["critical-section"] }
critical-section = "1.0"  # Your HAL typically provides the implementation
```

See [portable-atomic's documentation](https://docs.rs/portable-atomic) for details on supported targets and fallback mechanisms.

## Standards Compliance

- [RFC 9171](https://www.rfc-editor.org/rfc/rfc9171.html) - Bundle Protocol Version 7
- [RFC 9172](https://www.rfc-editor.org/rfc/rfc9172.html) - Bundle Protocol Security (BPSec)
- [RFC 9173](https://www.rfc-editor.org/rfc/rfc9173.html) - Default Security Contexts (behind feature flag)
- [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html) - Three-component IPN scheme

## Testing

- [Unit Test Plan](unit_test_plan.md) - RFC 9171 parsing, factories, EID logic
- [Fuzz Test Plan](fuzz_test_plan.md) - Bundle parsing, EID string/CBOR parsing
- [Component Test Plan](component_test_plan.md) - CLI-driven verification of library logic
- [BPSec Unit Test Plan](unit_test_plan_bpsec.md) - RFC 9172/3 Integrity & Confidentiality
