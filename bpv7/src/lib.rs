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
    .add_extension_block(block::Type::Payload)
    .with_flags(block::Flags {
        delete_bundle_on_failure: true,
        ..Default::default()
    })
    .build("Hello, world!")
    .build( CreationTimestamp::now());

assert_eq!(bundle.destination, destination);
assert!(!cbor.is_empty());
```

# Parsing Example

The following example demonstrates how to parse a BPv7 bundle from its CBOR representation.

```rust,cfg(feature = "std")
use hardy_bpv7::builder::Builder;
use hardy_bpv7::bundle::ValidBundle;
use hardy_bpv7::block;
use hardy_bpv7::creation_timestamp::CreationTimestamp;
use hardy_bpv7::eid::Eid;

// First, create a bundle to have something to parse.
let source: Eid = "ipn:1.0".parse().unwrap();
let destination: Eid = "ipn:2.0".parse().unwrap();
let (original_bundle, cbor) = Builder::new(source, destination.clone())
    .add_extension_block(block::Type::Payload)
    .with_flags(block::Flags {
       delete_bundle_on_failure: true,
       ..Default::default()
    })
    .build("Hello, world!")
    .build(CreationTimestamp::now());

// Parse the bundle from the CBOR data.
let parsed_bundle = ValidBundle::parse(&cbor, &hardy_bpv7::bpsec::key::EmptyStore).unwrap();

if let ValidBundle::Valid(bundle, _) = parsed_bundle {
    assert_eq!(bundle.id, original_bundle.id);
    assert_eq!(bundle.destination, original_bundle.destination);
} else {
    panic!("Bundle parsing did not result in a valid bundle");
}
```
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
pub mod error;
pub mod hop_info;
pub mod status_report;

pub use error::Error;
