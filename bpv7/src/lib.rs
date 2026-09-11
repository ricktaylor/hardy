/*!
A Rust implementation of the Bundle Protocol Version 7 (BPv7), as defined in [RFC 9171].

This crate provides the building blocks for working with BPv7 bundles, including creation, parsing, and manipulation.

# Key Modules

- [`bundle`]: Contains the structural [`Bundle`](bundle::Bundle) (primary block + blocks map) and its identifying types, including [`Bundle::semantic_eq`](bundle::Bundle::semantic_eq) for RFC-tolerant equivalence.
- [`parser`]: The streaming wire parser ([`parse`](parser::parse) / [`BundleParser`](parser::BundleParser)).
- [`checks`]: Composable BPSec validation primitives and the rewrite application step ([`apply_rewrites`](checks::apply_rewrites)).
- [`builder`]: Provides a [`Builder`](builder::Builder) for constructing new bundles.
- [`editor`]: Offers an [`Editor`](editor::Editor) for modifying existing bundles.
- [`eid`]: Implements Endpoint Identifiers (EIDs) as defined in BPv7.

# Usage Example

The following example demonstrates how to create a new BPv7 bundle with a payload. It calls [`CreationTimestamp::now`], so it requires the `std` feature.

```rust
use hardy_bpv7::{CreationTimestamp, builder::Builder, eid::Eid};

// EIDs can be created from strings.
let source: Eid = "ipn:1.0".parse().unwrap();
let destination: Eid = "ipn:2.0".parse().unwrap();

// Use the builder to construct a bundle.
let (bundle, cbor) = Builder::new(source.clone(), destination.clone())
    .with_report_to(source)
    .with_payload("Hello, world!".as_bytes().into())
    .build(CreationTimestamp::now())
    .unwrap();

assert_eq!(bundle.primary.destination, destination);
assert!(!cbor.is_empty());
```

# Parsing Example

The following example demonstrates how to parse a BPv7 bundle from its CBOR representation. It too requires the `std` feature, for [`CreationTimestamp::now`].

```rust
use hardy_bpv7::{CreationTimestamp, builder::Builder, eid::Eid, parser::parse};

// First, create a bundle to have something to parse.
let source: Eid = "ipn:1.0".parse().unwrap();
let destination: Eid = "ipn:2.0".parse().unwrap();
let (original_bundle, cbor) = Builder::new(source, destination.clone())
    .with_payload("Hello, world!".as_bytes().into())
    .build(CreationTimestamp::now())
    .unwrap();

// Structural parse: the cheapest entry point. Returns the authoritative
// byte buffer, the primary block + blocks map, and the decoded BPSec
// OperationSets. Slice with `&buf[block.payload_range()]`. Layer keyed
// BPSec validation on top by composing the primitives in
// `hardy_bpv7::checks` (`classify_*`, `decrypt_and_validate_covered_bibs`,
// `verify_all_bibs`, `apply_rewrites`, …).
let parsed = parse(bytes::Bytes::copy_from_slice(&cbor)).unwrap();

assert_eq!(parsed.bundle.primary.id, original_bundle.primary.id);
assert_eq!(parsed.bundle.primary.destination, original_bundle.primary.destination);
```

# `no_std` Support

This crate is `no_std` compatible with only a heap allocator required. Feature flags control
optional functionality:

- **`std`**: Enables system clock access and propagates `std` to dependencies.
- **`rfc9173`** (default): Enables RFC 9173 security contexts, which require random number generation.
- **`serde`**: Enables serialization support. Requires `std`.
- **`critical-section`**: Enables atomic fallback via `critical-section` for targets without native CAS (e.g., Cortex-M0).

## Embedded Targets

When using the `rfc9173` feature on embedded targets without OS-provided entropy, you must
provide a custom RNG backend. The `rand` dependency uses [`getrandom`] v0.4, which supports
[custom backends](https://docs.rs/getrandom/0.4/getrandom/#custom-backend) for targets not
supported out of the box. See `getrandom` v0.4 documentation for the target-specific
configuration required (typically a `RUSTFLAGS` override or a platform crate dependency).

[`getrandom`]: https://docs.rs/getrandom/0.4
[RFC 9171]: https://www.rfc-editor.org/rfc/rfc9171.html
*/
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

use alloc::{boxed::Box, vec::Vec};

#[cfg(feature = "std")]
use std::collections::{HashMap, HashSet};

#[cfg(not(feature = "std"))]
use hashbrown::{HashMap, HashSet};

pub mod bpsec;
pub mod builder;
pub mod bundle;
pub mod checks;
pub mod crc;
pub mod editor;
pub mod eid;
pub mod parser;
pub mod status_report;

mod bundle_age;
mod canonical;
mod creation_timestamp;
mod dtn_time;
mod error;
mod hop_info;

/// The types whose module holds nothing else, re-exported here with that
/// module kept private, so the crate root is their only path.
pub use self::{
    bundle_age::BundleAge,
    canonical::CaptureFieldErr,
    creation_timestamp::CreationTimestamp,
    dtn_time::DtnTime,
    error::{Error, Result},
    hop_info::HopInfo,
};
