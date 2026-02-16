/*!
Bundle Processing Agent library implementing RFC 9171.

This crate provides the core bundle processing logic, storage abstractions,
routing infrastructure, and service/CLA registries for a DTN node.

# `no_std` Support

This crate is `no_std` compatible with a heap allocator. Feature flags control functionality:

- **`std`**: Enables standard library support and propagates to dependencies.
- **`tokio`** (default): Enables Tokio runtime support. Implies `std`.
- **`rfc9173`**: Enables RFC 9173 security contexts via hardy-bpv7.
- **`serde`**: Enables serialization support for metadata.
- **`tracing`**: Enables tracing instrumentation.

## Current Limitations

Full `no_std` support is blocked by:
- `flume` (channel implementation) - std-only
- `metrics` (observability) - std-only

These are planned for future work with alternative implementations.

## Embedded Targets

When targeting embedded platforms without OS-provided entropy, you must provide a custom
RNG backend via the [`getrandom`](https://docs.rs/getrandom) crate's
[custom backend](https://docs.rs/getrandom/latest/getrandom/#custom-backend) mechanism.

For targets without native 64-bit atomics, enable the `critical-section` feature on
`hardy-bpv7` and provide a critical-section implementation from your HAL.

See the [hardy-bpv7 documentation](https://docs.rs/hardy-bpv7) for detailed instructions.
*/
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

mod dispatcher;
mod rib;

pub mod bpa;
pub mod bundle;
pub mod cla;
pub mod config;
pub mod filters;
pub mod keys;
pub mod metadata;
pub mod node_ids;
pub mod policy;
pub mod routes;
pub mod services;
pub mod storage;

// The generic error type
pub type Error = Box<dyn core::error::Error + Send + Sync>;

use alloc::sync::{Arc, Weak};
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "tracing")]
use tracing::instrument;

// Centralized collections for future no_std compatibility
// For no_std: HashMap/HashSet from hashbrown, BTreeMap/BTreeSet from alloc::collections
#[cfg(feature = "std")]
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map, hash_map};

#[cfg(not(feature = "std"))]
use hashbrown::{HashMap, HashSet, hash_map};

#[cfg(not(feature = "std"))]
use alloc::collections::{BTreeMap, BTreeSet, btree_map};

// Re-export for consistency
pub use bytes::Bytes;
pub use hardy_async::async_trait;
