# hardy-bpv7

Bundle Protocol Version 7 (RFC 9171) parser, builder, and editor with
BPSec (RFC 9172/9173) integrity and confidentiality support.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Installation

```toml
[dependencies]
hardy-bpv7 = "0.5"
```

Published on [crates.io](https://crates.io/crates/hardy-bpv7).

## Overview

hardy-bpv7 provides the core data structures and wire-format handling for
BPv7 bundles. It supports zero-copy parsing from canonical CBOR, a builder
for constructing new bundles, and an editor for modifying existing ones
(adding blocks, re-signing, re-encrypting). The crate is `no_std`
compatible with only a heap allocator required, making it suitable for
embedded DTN nodes.

## Features

- **Zero-copy parsing** -- bundles are parsed from CBOR without
  intermediate copies; block bodies reference the original buffer
- **Canonical CBOR** -- serialisation produces deterministic output per
  RFC 8949 Core Deterministic Encoding Requirements
- **CRC-16 / CRC-32** -- primary and extension block CRC validation and
  generation
- **EID support** -- IPN (3-element allocator-aware and legacy 2-element)
  and DTN naming schemes with `FromStr` / `Display`
- **Builder** -- fluent API for constructing bundles with payload, extension
  blocks, and BPSec security blocks
- **Editor** -- modify parsed bundles: add/remove blocks, update fields,
  re-compute CRCs and security results
- **Bundle status reports** -- parse and generate administrative records
  per RFC 9171 Section 6.1
- **Hop count and age tracking** -- extension block support per RFC 9171
  Sections 4.3.3 and 4.3.4

### BPSec (RFC 9172 / 9173)

Enabled by default via the `rfc9173` feature flag:

- **BIB-HMAC-SHA2** -- Block Integrity Block with HMAC-SHA-256/384/512
- **BCB-AES-GCM** -- Block Confidentiality Block with AES-128/256-GCM
- **Key wrap** -- AES-KW key wrapping for content-encryption keys
- **KeySet** -- key material container for verification and decryption

## Feature Flags

| Flag | Default | Description |
|------|---------|-------------|
| `rfc9173` | yes | RFC 9173 security contexts (BIB-HMAC-SHA2, BCB-AES-GCM, key wrap). Enables `hmac`, `sha2`, `aes-gcm`, `aes-kw`, `rand` |
| `std` | no | Enables system clock access (`CreationTimestamp::now()`) and propagates `std` to dependencies |
| `serde` | no | Enables `Serialize`/`Deserialize` on bundle types. Requires `std` |
| `bpsec` | no | Internal: enables BPSec signing/encryption modules. Automatically enabled by `rfc9173` |
| `critical-section` | no | Atomic fallback via `portable-atomic` for targets without native CAS (e.g. Cortex-M0) |

## Usage

```rust
use hardy_bpv7::builder::Builder;
use hardy_bpv7::eid::Eid;
use hardy_bpv7::creation_timestamp::CreationTimestamp;

let source: Eid = "ipn:1.0".parse().unwrap();
let destination: Eid = "ipn:2.0".parse().unwrap();

// Build a bundle
let (bundle, cbor) = Builder::new(source, destination)
    .with_payload("Hello, world!".as_bytes().into())
    .build(CreationTimestamp::now())
    .unwrap();

// Parse it back
use hardy_bpv7::bundle::ParsedBundle;
let parsed = ParsedBundle::parse(&cbor, hardy_bpv7::bpsec::no_keys)
    .unwrap()
    .bundle;
assert_eq!(parsed.id, bundle.id);
```

### Embedded / `no_std`

When using `rfc9173` on embedded targets without OS-provided entropy, you
must provide a custom RNG backend. The `rand` dependency uses `getrandom`
v0.4, which supports
[custom backends](https://docs.rs/getrandom/0.4/getrandom/#custom-backend)
for targets not supported out of the box.

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [API Documentation](https://docs.rs/hardy-bpv7)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
