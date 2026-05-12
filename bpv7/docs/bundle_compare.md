# Bundle Comparison Design

Semantic comparison of BPv7 bundles, accounting for the encoding freedoms in RFC 9171, RFC 9172, and RFC 9173.

## Comparison modes

The comparison supports two modes:

- **Strict**: the bundles must be *identical*: two different encodings of the same bundle. Only CBOR encoding freedoms are tolerated (definite vs indefinite length, block ordering, block number assignment, ASB target/parameter/result ordering). CRC type, security parameters, and security results must all match.

- **Relaxed**: the bundles must be *equivalent*: same semantic content, but implementation choices (CRC type) are tolerated.

## Strict comparison

### Precondition

The two bundles are two different encodings of the same bundle, not two independently processed bundles. The only differences between them are encoding freedoms.

Two identical bundles that independently undergo the same transformation are no longer identical:
- Security operations are non-deterministic. The same operation (same key, same context) produces different results because of random IV. BIB signatures over encrypted content also differ since the ciphertext they cover differs.
- CRC type changes recompute the CRC value and modify the block encoding.

The strict comparison does not decrypt or reverse any transformation. It compares the bundle as-is.

### Strategy

All blocks are compared using parsed content, not raw bytes. This handles CBOR encoding differences (definite vs indefinite length) transparently.

| Block | Tolerated | Normalized | Excluded |
|-------|-----------|------------|----------|
| Primary | - | - | - |
| Payload | - | - | - |
| Extension blocks | Block number, block ordering | - | - |
| BIB / BCB | Block number, block ordering, target/parameter/result array ordering (RFC 9172 Section 3.6) | - Absent parameters are filled with their default value before comparison (RFC 9173 Section 3.3) <br> - Target block numbers are translated from raw block numbers to (block_type, index) before comparison ([details](#target-resolution)) | - |

## Relaxed comparison

### Precondition

Same as strict, but CRC type differences are tolerated.

When a security block is removed, the target block's CRC must be restored but the original CRC type is not preserved. The restoring node picks a CRC type based on local policy (RFC 9173 Section 3.8.2, 4.8.2). Two implementations processing the same bundle may restore different CRC types.

### Strategy

Same as strict. No decryption, no keys needed.

| Block | Tolerated | Normalized | Excluded |
|-------|-----------|------------|----------|
| Primary | - | - | CRC type |
| Payload | - | - | CRC type |
| Extension blocks | Block number, block ordering | - | CRC type |
| BIB / BCB | Block number, block ordering, target/parameter/result array ordering (RFC 9172 Section 3.6) | - Absent parameters are filled with their default value before comparison (RFC 9173 Section 3.3) <br> - Target block numbers are translated from raw block numbers to (block_type, index) before comparison ([details](#target-resolution)) | CRC type |

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
