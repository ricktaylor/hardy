/*!
A Rust implementation of the Bundle Protocol Version 7 (BPv7), as defined in [RFC 9171](https://www.rfc-editor.org/rfc/rfc9171.html).

This crate provides the building blocks for working with BPv7 bundles, including creation, parsing, and manipulation.

# Key Modules

- [`bundle`]: Contains the primary [`Bundle`](bundle::Bundle) struct and related components.
- [`builder`]: Provides a [`Builder`](builder::Builder) for constructing new bundles.
- [`editor`]: Offers an [`Editor`](editor::Editor) for modifying existing bundles.
- [`eid`]: Implements Endpoint Identifiers (EIDs) as defined in BPv7.
- [`block`]: Defines the structure of blocks within a bundle.

# Usage Example

The following example demonstrates how to create a new BPv7 bundle with a payload.

```rust,cfg(feature = "std")
use hardy_bpv7::builder::Builder;
use hardy_bpv7::block;
use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;

// EIDs can be created from strings.
let source: Eid = "ipn:1.0".parse().unwrap();
let destination: Eid = "ipn:2.0".parse().unwrap();

// Use the builder to construct a bundle.
let (bundle, cbor) = Builder::new(source.clone(), destination.clone())
    .with_report_to(source)
    .with_payload("Hello, world!".as_bytes().into())
    .build(CreationTimestamp::now())
    .unwrap();

assert_eq!(bundle.destination, destination);
assert!(!cbor.is_empty());
```

# Parsing Example

The following example demonstrates how to parse a BPv7 bundle from its CBOR representation.

```rust,cfg(feature = "std")
use hardy_bpv7::builder::Builder;
use hardy_bpv7::bundle::ParsedBundle;
use hardy_bpv7::block;
use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;

// First, create a bundle to have something to parse.
let source: Eid = "ipn:1.0".parse().unwrap();
let destination: Eid = "ipn:2.0".parse().unwrap();
let (original_bundle, cbor) = Builder::new(source, destination.clone())
    .with_payload("Hello, world!".as_bytes().into())
    .build(CreationTimestamp::now())
    .unwrap();

// Parse the bundle from the CBOR data (no keys needed for this bundle).
let bundle = ParsedBundle::parse(&cbor, hardy_bpv7::bpsec::no_keys).unwrap().bundle;

assert_eq!(bundle.id, original_bundle.id);
assert_eq!(bundle.destination, original_bundle.destination);

// Alternatively, if you have a KeySet for decryption/verification:
let keys = hardy_bpv7::bpsec::key::KeySet::EMPTY;
let bundle2 = ParsedBundle::parse_with_keys(&cbor, &keys).unwrap().bundle;
```

# `no_std` Support

This crate is `no_std` compatible with only a heap allocator required. Feature flags control
optional functionality:

- **`std`**: Enables system clock access and propagates `std` to dependencies.
- **`rfc9173`** (default): Enables RFC 9173 security contexts, which require random number generation.
- **`serde`**: Enables serialization support. Requires `std`.

## Embedded Targets

When using the `rfc9173` feature on embedded targets without OS-provided entropy, you must
provide a custom RNG backend. The cryptographic dependencies use the [`getrandom`] crate,
which supports [custom backends](https://docs.rs/getrandom/latest/getrandom/#custom-backend)
for targets not supported out of the box.

To use a custom entropy source:

1. Add `getrandom` with the `custom` feature to your `Cargo.toml`
2. Implement the `__getrandom_v03_custom` function to provide entropy from your hardware RNG

See the [design documentation](https://github.com/example/hardy/blob/main/bpv7/docs/design.md#embedded-targets-and-custom-rng)
for detailed instructions.

[`getrandom`]: https://docs.rs/getrandom
*/
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec::Vec,
};

#[cfg(feature = "std")]
use std::collections::{HashMap, HashSet};

#[cfg(not(feature = "std"))]
use hashbrown::{HashMap, HashSet};

pub mod block;
pub mod bpsec;
pub mod builder;
pub mod bundle;
pub mod crc;
pub mod creation_timestamp;
pub mod dtn_time;
pub mod editor;
pub mod eid;
pub mod hop_info;
pub mod status_report;

mod error;
pub use error::Error;
