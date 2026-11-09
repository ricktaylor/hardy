# hardy-bpv7 Design

Bundle Protocol version 7 implementation per [RFC 9171](https://www.rfc-editor.org/rfc/rfc9171.html).

## Design Goals

- **Zero-copy parsing.** Bundle data is parsed in place, with structures holding byte ranges into the source buffer rather than copied data. This enables efficient payload access and CRC computation without duplication.

- **Deterministic encoding.** The library always produces deterministic CBOR output per RFC 8949 §4.2.1 (sometimes called "canonical" encoding) and rejects input that violates it, accepting only the indefinite-length items RFC 9171 §4.1 permits.

- **Type-safe EID representation.** Endpoint identifiers are represented as distinct enum variants reflecting their semantic differences, not just string parsing. `LocalNode` is a separate variant because [RFC 9758](https://www.rfc-editor.org/rfc/rfc9758.html) defines it as a distinct concept.

- **Separation of mechanism and policy.** The library performs parsing, validation, and transformation but doesn't impose policies. Key providers, BPSec failure handling, and block handling are all caller decisions.

- **`no_std` compatibility.** The core library works on embedded platforms with only a heap allocator, though some features (system clock, serde) require `std`.

## Parsing Modes and Trust Boundaries

Parsing is layered rather than moded, so each caller composes exactly the processing it needs; the [Parse & Validation Pipeline Design](parser_design.md) is the full reference.

The **structural parse** (`parse::parse`, or `parse::BundleParser` driven incrementally over a stream) is keyless: it decodes the primary block, the block map, and the BPSec OperationSets, verifies CRCs, and rejects malformed or non-deterministically encoded input. A streamed parse can stop once the header blocks are resident and hand back a `PayloadTail` continuation that validates the payload's CRC and trailer as the remaining bytes arrive.

**Keyed validation** (`bpsec::checks`) layers BPSec on top, as a method on `Parsed`: the parser having classified the blocks the node cannot process by their flags (`Parsed::classification`), the security operations are checked with a caller-supplied `KeySource` in this node's roles on each block (`check`): the acceptor's decryption, the verifier's decryption of its own block and the integrity checks, in one pass. On a fragment the check processes the per-hop blocks' operations alone, the forwarder's writes to the fragment, and defers every other, looking up no key for it, since it is checkable only once the bundle is reassembled (RFC 9172 §5.2). The checks report facts — decrypted, no key, decryption failed, coverage — and never decide accept-or-reject themselves.

**Rewrite application** (`rewrite::apply_rewrites`) applies the block removals and canonical re-emits the caller has decided on, through the BPSec-aware editor cascade.

The policy that distinguishes trust levels — which failures reject a bundle, whether to honour a block-removal request — lives in the caller, so the BPA, the CLI tools, and the fuzz harness share one implementation.

## Zero-Copy Architecture

Bundles are parsed in place with structures holding `Range<u64>` values — bundle-absolute byte offsets — into the source byte stream. A `Block` doesn't contain its payload - it contains the byte range where the payload lives.

This design serves several purposes. First, large payloads aren't copied during parsing. Second, CRC validation can hash the exact byte ranges without reassembly. Third, and importantly, Range values are "recipes" rather than views - they describe where data lives without requiring it to be in memory. This creates future capability for lazy loading where only portions of a bundle are fetched from storage as needed. Only the payload body may be large: the parser bounds everything before it — the primary block, every extension block and the payload block's header — to 256 MiB, rejecting a longer declaration from the block lengths alone. This is an implementation limit, not an RFC one: real header chains are kilobytes, the pre-payload region must be resident for verification, and a fixed bound keeps accept/reject decisions identical on every node regardless of pointer width.

When blocks are encrypted by a BCB, the decrypted content must be stored somewhere. The `Payload` enum handles this with two variants: `Borrowed` (a reference into the original buffer) and `Decrypted` (owned data that's automatically zeroed when dropped). This maintains the zero-copy model for unencrypted blocks while properly handling decrypted content.

## Builder and Editor

The library provides two patterns for bundle construction.

**Builder** is a factory for creating new bundles from scratch. It uses a fluent API where callers specify source, destination, lifetime, flags, and payload, then call `build()` to produce a complete bundle and its CBOR encoding. For a payload supplied as a stream, `build_stream()` is the emit-side twin of the streaming parser: it returns the bundle view, the resident wire prefix ending at the payload block's byte-string head, and a `PayloadTrailer` that absorbs the streamed payload into the payload block's CRC and then emits the CRC field and the closing break. Prefix, payload, and trailer concatenate to exactly the `build()` output, so the payload never has to be resident to build the bundle.

**Editor** is for modifying existing bundles. It's optimised for the forwarding case where most of the bundle stays the same. Rather than re-encoding everything, Editor tracks what changed and surgically updates only the affected portions. For a forwarding node adding Previous Node and incrementing Hop Count, this avoids re-encoding the entire payload.

Both can frame a payload that streams rather than sitting in memory: `Builder::build_stream` for a bundle being originated, and `Editor::with_streamed_payload` with `Editor::rebuild_streamed` for a received one whose payload's acceptor decrypts it as it streams. Each returns the headers and the payload block's header for the declared length, and a `PayloadTrailer` that is fed the payload and ends the wire form; the two share that framing, so they agree byte for byte.

The distinction matters for performance. A node forwarding thousands of bundles per second benefits significantly from patching two fields versus re-encoding megabytes of payload data.

## BPSec Integration

The library implements RFC 9172 (Bundle Protocol Security) with a pluggable architecture for security context providers.

### Pluggable Security Contexts

Security contexts are compiled in: each is a feature-gated variant of the BIB and BCB operation enums, and an operation whose context this build does not include parses as `Unrecognised` and is classified as unsupported rather than failing the parse. Key management is the pluggable part: when processing BIBs and BCBs, the library asks a caller-supplied `KeySource` for the key for each operation. This separation allows:

- Different deployments to use different key management systems
- Feature-flagged inclusion of specific contexts (e.g., RFC 9173 default contexts)

The library exposes a common key representation based on JWK (JSON Web Key) format, providing flexible data types for managing keychains.

### Security Block Structure

Following RFC 9172's design, security operations within a BIB or BCB share context parameters but produce unique results per target. The library reflects this by using reference-counted parameters shared across operations while maintaining separate result storage for each target block. This matches the wire format where a single security block can protect multiple targets with one set of parameters but distinct cryptographic results.

### Operations, Acceptance and Removal

A security operation is one security service applied to one security target, OP(service, target) (RFC 9172 §1.4): one entry in a BIB's or a BCB's target list, with its results. Each is unique in a bundle (§3.2), so a block is the target of at most one BIB operation and one BCB operation, and a security block is itself a block, the target of a BCB operation when it is encrypted. Acceptance and removal act on operations, never on a security block as a whole. Removing an operation removes its entry from its security block, and the security block goes only when its last operation goes: a cascading delete, which removes in turn the operation that targets that block, the BCB operation on an encrypted BIB, whose BCB may then empty and go the same way.

Removing a block and accepting one walk the same references, from a block to the operations that target it, from an operation to its security block, and from an emptied security block to the operation that targets it. Removing a block removes every operation that targets it and cascades as above (`BPSecEditor::remove_blocks`). Accepting a block is the security acceptor's walk (§5.1.1, §5.1.2): it processes and removes every operation that targets the block, decrypting the BCB operation and verifying the BIB operation over the plaintext. Where the BIB is encrypted, reading it accepts the BCB operation on it too, and that goes with the BCB operations on the BIB's other targets, all or none, so that no reader meets a plaintext BIB over ciphertext (§3.9): the walk accepts each of those targets with the block, its BIB operation too, so the BIB empties, and the cascade removes it and any BCB it empties. A caller that does not accept every block the walk reaches can accept none of them (`Parsed::check` reports the block `incomplete`), and `bpsec::edit::remove_encryption` removes the coupled operations together. Neither walk ever encrypts anything again: shrinking an encrypted BIB that also covers a block that stays would mean writing new ciphertext under its BCB's security source, which only that source may do, and under RFC 9173, whose IV belongs to the whole BCB, cannot be done soundly for a BIB that shares its BCB. So `remove_blocks` refuses such a removal, naming the block (`editor::Error::ShrinksEncryptedBib`), as it refuses one that would orphan a block an unreadable encrypted BIB may list, or restore the primary's CRC under an operation that scopes it; it removes every requested block or none. Naming the encrypted BIB too removes it whole, its BCB operation with it.

The table gives what each walk does to a block A, in every security context:

| A's protection | Accepting A | Removing A |
|---|---|---|
| A BCB operation, no BIB | Decrypt A and remove its BCB operation | Remove its BCB operation |
| A plaintext BIB, covering A alone or with others | Verify A and remove its BIB operation | Remove its BIB operation |
| A BCB operation and an encrypted BIB covering A alone | Decrypt A and the BIB, verify, and remove all three operations: the BIB empties | Remove A: the BIB empties and goes unread |
| A BCB operation and an encrypted BIB also covering B, which goes too | As above, for A and B together, all or none | As above, for A and B together |
| A BCB operation and an encrypted BIB also covering B, which stays | Refused: an incomplete acceptance | Refused: the BIB would shrink |
| An encrypted BIB this node cannot read may cover A (`Maybe`) | Refused: an incomplete acceptance | Refused while that BIB stays |

Removing an operation removes its security block when it was the last; the rows' removals cascade so.

As built under §3.9, an encrypted BIB's targets are among its BCB's: a BCB added over all of a BIB's targets encrypts that BIB too, and one added over only some of them splits the BIB, moving the matched results into a new BIB that is encrypted with them. The BCB may target more. A reader that cannot decrypt an encrypted BIB so knows only that the BIB covers some of its BCB's other targets, yet the parser marks every BCB-covered block as maybe covered, not just that BCB's: acceptance is per operation, so an acceptor elsewhere may remove a target's BCB operation and leave the encrypted BIB over it, after which a later node may encrypt that target under another BCB, and a sender that does not follow §3.9 may build the same. Hardy never leaves that state: it removes coupled operations together.

### Editing Under BPSec

An operation can cover the primary block without targeting it: RFC 9173's default scope puts the primary in every BIB's integrity-protected plaintext and every BCB's additional authenticated data. Each security context reports, from its own parameters, whether an operation's scope includes the primary (`scope_includes_primary()` on `bib::Operation` and `bcb::Operation`); an unrecognised context counts as including it, and a consumer such as the BPA never reads a context's parameters itself. The owner `Editor` refuses a primary-block edit while a BIB targets the primary or any remaining operation's scope includes it, an unreadable BIB counting as one. No security operation changes the primary: it is immutable (RFC 9171 §4.3.1), so signing it and removing its BIB leave it as it stands, CRC included, and every other operation that covers it keeps verifying. Fragment info is exempt: no BIB or BCB is added to a fragment (RFC 9172 §5.2), so every operation covers the unfragmented primary.

No security operation changes a target's CRC, on any block, under any context. RFC 9172 §9.3 leaves a target's CRC to the security context's own statement, and RFC 9173's contexts have the security source remove it and a security acceptor short of the destination include one again (§3.8.1, §3.8.2, §4.8.1, §4.8.2); bpv7 does neither, as the draft COSE context does not. The CRC is in neither the IPPT (RFC 9173 §3.7) nor the AAD (§4.7), so an operation's result is the same with it or without it; a CRC the source removes is the one check every node between it and the acceptor can make (RFC 9171 §5.6); and the primary block is immutable, its CRC covered by every operation whose scope includes it, so each change to it broke those operations. The Signer and the Encryptor leave a target's CRC type as it stands, the Encryptor's rebuild computing the CRC over the ciphertext as any block edit does; verification and decryption take a target with a CRC or without one; and the editor's removals add none, so a target whose source removed its CRC travels on without one.

### Processing Order

Bundle security processing follows a fixed order, because verification reads plaintext that decryption recovers.

First, all blocks are parsed and BCB targets are marked. Without keys the parser cannot read the target list of a BCB-encrypted BIB, so it marks every block such a BIB might cover as "maybe covered".

Second, BCB-encrypted BIBs are decrypted and validated, replacing the "maybe" marks with their real coverage.

Third, the blocks the consumer accepts for confidentiality are decrypted: at a receiver the per-hop blocks, every Previous Node and Bundle Age block and a Hop Count it holds the key for, and at the ingress gate the blocks its policy announces. An encrypted BIB over any of them is read by necessity, and the blocks coupled through it are taken all or none. A block the consumer verifies for confidentiality is decrypted alone, into the cache, the block travelling as received.

Fourth, every BIB operation on a block the consumer names for integrity, as its acceptor or its verifier, is verified where its target is readable, reading the plaintext the decryption cached: verification follows decryption, and `Parsed::check` runs the two in that order in one pass, over the roles the consumer names by block number (`bpsec::Roles`) and its roles on every other block. A BIB whose payload target is not resident when the headers are verified, or lies under a BCB, is deferred to the payload drain, its key required now.

The payload's own BCB is not part of these passes, since the payload need not be resident. With the payload's confidentiality accepted, `Parsed::check` settles everything about its BCB operation that needs a key before the first byte, and the BIB over it the same way: the keys are resolved, the operations recorded deferred, and an encrypted BIB over the payload is decrypted, so its operation can be checked against the plaintext. Its security acceptor decrypts it as it streams: `bcb::Operation::begin_decrypt` begins a `bcb::Decryptor` from the headers alone, resolving the key before the first byte, and the plaintext it returns at `finish`, authenticated, is what a BIB over the payload is checked against (`bib::Operation::begin_verify`, fed the plaintext). bpv7 supplies these per-operation stages; which operations to begin, and what a missing key means, is the consumer's policy, so the BPA composes them. Under BCB-AES-GCM the decryptor collects the ciphertext and decrypts it whole when finished; an incremental AES-GCM can replace that behind the same interface. A consumer reading a resident encrypted payload decrypts it through `bpsec::DecryptingReader`.

### Future Work: COSE Security Contexts

The pluggable security context architecture is designed to support additional security contexts as they reach standardisation. The BPSec COSE context (draft-ietf-dtn-bpsec-cose) is a stretch goal that will be integrated once the specification stabilises, following the same pattern as the RFC 9173 contexts.

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

RFC 9171 §4.1 requires the CBOR encodings of all fields in all blocks to conform to the core deterministic encoding requirements of RFC 8949 §4.2.1 (sometimes referred to as "canonical" encoding), "except that indefinite-length items are not prohibited". The library's approach is:

1. Always produce deterministic output
2. Reject non-deterministic input as a hard parse error (`Error::NotCanonical`) rather than repairing it: a non-shortest integer or length, or a tag where the grammar permits none, fails the parse
3. Accept the indefinite-length items §4.1 permits

Rejecting rather than repairing keeps every byte a node relays identical to the bytes it received and verified. BPSec computes integrity over canonical forms (RFC 9172 §4), so repairing non-deterministic input in place would forward bytes that differ from what arrived and was checked; and silently reserialising another implementation's bundle would hide that implementation's encoder bugs from its sender, where a rejection surfaces them.

The primary block records whether it arrived in canonical form, so BPSec can re-emit that form when computing over it (RFC 9172 §4). Extension-block bodies whose fields the consumer decodes can still be queued for a canonical re-emit through `rewrite::apply_rewrites`.

## Block Handling

Extension blocks use the same zero-copy approach as the primary block. Each `Block` structure contains its type, flags, CRC information, and byte ranges into the source data.

The `BibCoverage` enum tracks integrity protection state: `None` (no BIB covers this block), `Some(block_number)` (protected by a specific BIB), or `Maybe` (encrypted BIBs couldn't be decrypted, so coverage is unknown). This three-state model allows code to distinguish between "definitely unprotected" and "unknown protection status."

Unrecognised blocks are handled per RFC 9171 rules. The parser's keyless classification (`Parsed::classification`) reports the unprocessable blocks flagged "delete block if block can't be processed", and the deletion any flagged "delete bundle" demands; the caller decides whether to honour the removals, which `rewrite::apply_rewrites` applies. The library rewrites BIBs and BCBs that targeted removed blocks, potentially removing security blocks entirely if all their targets are gone.

## Utility Types

The library provides type-safe representations of the various fields and structures defined in RFC 9171. These types enforce correctness through the Rust type system rather than relying on runtime validation of raw values.

Key utility types include `CreationTimestamp` (handling both clocked and clockless bundle creation), `DtnTime` (DTN epoch-based timestamps with conversion helpers), `HopInfo` (hop limit and count), `BundleAge` (elapsed time since creation for clockless nodes), `FragmentInfo` (ADU fragmentation), and `bundle::Id` (unique bundle identification with serialization support for database keying). Flag fields from the primary block and extension blocks are represented as type-safe structures rather than raw bitmasks. The `status_report` module provides `BundleStatusReport` and related types for generating and parsing RFC 9171 administrative records, including the full set of reason codes.

These types implement the `ToCbor` and `FromCbor` traits for wire format encoding, and optionally `serde` traits for metadata persistence. Full API documentation is available via rustdoc.

## Integration

### With hardy-cbor

The library builds on hardy-cbor's wire-format parsing. It uses closure-based array parsing for structural integrity, Range returns for zero-copy access, and `shortest` flag propagation for deterministic encoding detection. The separation is deliberate: hardy-cbor handles CBOR-level concerns while hardy-bpv7 handles bundle-level semantics.

### With hardy-bpa

The Bundle Processing Agent composes the structural parser, `bpsec::checks`, and `rewrite` with its own policy — which failures reject a bundle, the NoKey disposition, the status-report reason mapping — in its `bundle::arrival` module. At ingress it parses the header blocks off the arriving stream, verifies them before anything is stored, and drains the payload through the `PayloadTail` continuation.

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

The `rfc9173` feature requires random number generation for cryptographic operations (key generation, nonces). This is provided by the `getrandom` crate, which uses OS-provided entropy by default.

See [getrandom's documentation](https://docs.rs/getrandom/latest/getrandom/) for the full list of supported targets and custom backend details.  For embedded targets without OS RNG support, you must provide a custom entropy source using getrandom's [custom backend](https://docs.rs/getrandom/latest/getrandom/#custom-backend):

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
