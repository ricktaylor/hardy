# hardy-custody-receipt

A custodian for the Custody Receipt Block ([draft-bsvkey-dtn-custody-receipt-block-01](https://datatracker.ietf.org/doc/draft-bsvkey-dtn-custody-receipt-block/)): an Egress `Rewriter` for the Hardy BPA that signs a version 2 custody receipt for each bundle the node forwards and appends it to the bundle's Custody Receipt Block (experimental block type 224), adding the block when it is absent.

A receipt is an untagged COSE_Sign1 (RFC 9052) with EdDSA over Ed25519, whose payload is a CBOR map: the previous hop, this custodian's EID, the BPv7 bundle identity, a SHA-256 of the payload block's data as held, and the reception and forwarding times. Nodes that do not run the filter forward the block unchanged.

## Scope

- Edits only block 224, through the BPA's scoped `ExtensionEditor`. The payload, the primary block and every other block are never modified.
- Reads the payload only to hash the bytes held. A BCB-covered payload is left unsigned, with an info-level log line: a receipt binds the payload bytes as held (the ciphertext), and a Rewriter reads block bodies only through the decrypting reader, which yields plaintext with a key and no bytes without one.
- Copies earlier receipts byte for byte, and refuses (leaves the bundle as it is) a block holding version 1 receipts, mixed versions, malformed data, or 64 receipts already.
- The previous hop is the Previous Node block when present, else the CLA's ingress peer, else, for a bundle sourced at this node, its source.
- Administrative records are not signed. A block under BPSec coverage is not edited (the editor refuses it).

## Configuration

Built into `hardy-bpa-server` with the `custody-receipt` feature:

```yaml
custody-receipt:
  eid: "ipn:20.0"              # this custodian's EID, as verifiers pin it
  key-file: "/etc/hardy/custody.seed"  # Ed25519 seed, 64 hexadecimal digits
  policy: always               # or block-present: sign only bundles that already carry block 224
```

## Status

Prototype. The receipt encoding is checked byte for byte against the JavaScript reference implementation (`tests/js_vector.rs`), and a live two-way run (Hardy signs, the reference verifies; the reference signs, Hardy keeps it and appends) passes. Block type 224 is from the private and experimental range until a code point is assigned.

## Licence

Apache-2.0, as the rest of the workspace.
