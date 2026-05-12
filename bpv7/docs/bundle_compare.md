# Bundle Comparison Design

Bundle encoding is not deterministic. RFC 9171, 9172, and 9173 allow multiple valid encodings for the same bundle: CBOR definite vs indefinite length, arbitrary block numbering and ordering, security target array ordering, and optional parameter elision. Two bundles with completely different bytes can represent the same content.

This module determines if two bundles are identical by comparing parsed content, since byte-level comparison is unreliable given the encoding freedoms defined in the RFCs.

## Precondition

The comparison assumes the two bundles originate from the same bundle. They may have been re-encoded differently, but the content (including security operations) is the same. The comparison handles encoding freedoms, not content differences.

Security operations are non-deterministic transformations: applying the same operation (same key, same context) independently produces different results because of random IV. Two bundles that were independently signed or encrypted are no longer the same bundle, even if they started from the same content.

The comparison does not decrypt or reverse any transformation. It compares the bundle as-is.

## Strategy

All blocks are compared using parsed content, not raw bytes. This handles CBOR encoding differences (definite vs indefinite length) transparently.

CRC type (CRC-16 vs CRC-32) is an implementation choice (RFC 9171 Section 4.2.1). The comparison only checks whether CRC is present or absent, not the specific type. When a security block is removed, the target block's CRC must be restored but the original CRC type is not preserved; the restoring node picks a type based on local policy (RFC 9173 Section 3.8.2, 4.8.2).

| Block | Tolerated | Normalized | Excluded |
|-------|-----------|------------|----------|
| Primary | CRC type (presence checked) | - | - |
| Payload | CRC type (presence checked) | - | - |
| Extension blocks | Block number, block ordering, CRC type (presence checked) | - | - |
| BIB / BCB | Block number, block ordering, CRC type (presence checked), target/parameter/result array ordering (RFC 9172 Section 3.6) | - Absent parameters are filled with their default value before comparison (RFC 9173 Section 3.3) <br> - Target block numbers are translated from raw block numbers to (block_type, index) before comparison ([details](#target-resolution)) | - |

## Block pairing

Extension blocks are grouped by block type. Within each type, blocks are sorted by block number to establish a canonical ordering within each bundle, then paired positionally (first-to-first, second-to-second). Block numbers themselves are not compared.

## Target resolution

Security blocks reference targets by block number, but block numbers are arbitrary identifiers (RFC 9171 Section 4.1), except 0 (primary) and 1 (payload) which are fixed.

To compare targets across bundles that may assign different block numbers:

1. In each bundle, build a mapping from block number to `(block_type, index)`, where `index` is the block's position within all blocks of that type, sorted by block number.
2. Translate each security target from a raw block number to its `(block_type, index)` tuple.
3. Compare the translated target sets.

This handles the common case where the same blocks exist in both bundles but are assigned different block numbers. It assumes that blocks of the same type appear in the same relative order (by block number) in both bundles.

Limitation: if two bundles have multiple blocks of the same type in a different relative order, and a security block targets one of them, the positional matching may pair the wrong blocks. Content-based matching would be needed to resolve this, but is not feasible for encrypted targets. This is an accepted limitation.
