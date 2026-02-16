# Making `bpa` no_std Compatible

**Last Updated:** 2026-02-16

This document outlines the work required to make the `bpa` package `no_std` compatible as an optional feature.

**Related Document:** See `/workspace/async/HARDY_ASYNC_PROPOSAL.md` for the runtime abstraction strategy and current migration status.

**Scope:** This document covers only the `bpa` crate. Higher-level packages (bpa-server, tcpclv4, proto, etc.) are out of scope for no_std and remain tokio-dependent.

**Target:** `no_std` + `alloc` is the only viable path forward. Pure `no_std` (no heap) is not practical for a BPA implementation due to dynamic data structures (bundles, routing tables, registries). Custom allocators in Rust are not yet mainstream, and embedded platforms like Embassy do not yet have stable support for them. Once custom allocators mature, more fine-grained memory control may become possible.

## Current State Summary

| Category | Status | Severity |
|----------|--------|----------|
| Async Runtime (`hardy-async`) | **bpa fully abstracted** - zero direct tokio deps, `std` feature added | DONE |
| Cargo.toml Feature Flags | `std` feature with proper dependency forwarding, tokio implies std | DONE |
| Collections | HashMap/HashSet gated (std/hashbrown), BTreeMap/BTreeSet from alloc | DONE |
| Arc/Weak | Using `alloc::sync::{Arc, Weak}` | DONE |
| Time Types | `core::time::Duration` for durations | DONE |
| Error handling | Using `core::error::Error` (requires Rust 1.81+) | DONE |
| std:: to core:: migration | Migrated (fmt, cmp, ops, mem, num, etc.) | DONE |
| Prelude consistency | Simplified where possible (Default, Result) | DONE |
| Synchronization (Mutex/RwLock) | `hardy_async::sync` wrappers available, bpa migrated | DONE |
| Synchronization (Once) | `hardy_async::sync::spin::Once` available | DONE |
| Allocator | Requires `alloc` (Vec, String, Arc, Box) | EXPECTED |

### Minimum Rust Version

The workspace requires **Rust 1.85** due to:
- Edition 2024
- `core::error::Error` (stabilized in Rust 1.81)

This is set in `Cargo.toml` via `rust-version = "1.85"`.

---

## What's Complete

### Async Runtime Abstraction

The `bpa` crate now has **zero direct tokio dependencies**. All async primitives are accessed through `hardy-async` abstractions:

| Primitive | hardy-async Abstraction | Status |
|-----------|------------------------|--------|
| `tokio::spawn` | `hardy_async::spawn!` macro | DONE |
| `tokio::task::JoinHandle` | `hardy_async::JoinHandle` | DONE |
| `tokio::task::JoinSet` + `Semaphore` | `hardy_async::BoundedTaskPool` | DONE |
| `tokio::sync::Notify` | `hardy_async::Notify` | DONE |
| `tokio::time::sleep` | `hardy_async::time::sleep` | DONE |
| `tokio::select!` | `futures::select_biased!` | DONE |
| `tokio_util::CancellationToken` | `hardy_async::CancellationToken` | DONE |

This means bpa is ready for Embassy support once hardy-async has Embassy backends.

### Synchronization Abstraction

The `hardy-async` crate now provides synchronization primitives:

| Primitive | Location | Use Case |
|-----------|----------|----------|
| `sync::Mutex` | `hardy_async::sync::Mutex` | O(n) operations, may block |
| `sync::RwLock` | `hardy_async::sync::RwLock` | O(n) read-heavy, may block |
| `sync::spin::Mutex` | `hardy_async::sync::spin::Mutex` | O(1) hot paths, no blocking |
| `sync::spin::RwLock` | `hardy_async::sync::spin::RwLock` | O(1) read-heavy hot paths |

**bpa Usage:**
- `cla/peers.rs`: Uses `hardy_async::sync::spin::RwLock` for PeerTable (O(1) HashMap ops, hot forwarding path)
- `cla/registry.rs`: Uses `hardy_async::sync::spin::Mutex` for CLA HashMap (O(1) lifecycle operations)

**Note:** `Once` is now available via `hardy_async::sync::spin::Once` as a wrapper around `spin::once::Once`.

### Dispatcher Refactoring

The `Dispatcher` previously used `OnceLock` for deferred initialization. This has been replaced with a "return closure" pattern that avoids both OnceLock and the race conditions of `Arc::new_cyclic`:

```rust
pub fn new(config: &Config, ...) -> Arc<Self> {
    let (dispatcher, start) = Self::new_inner(config, ...);
    start(&dispatcher);
    dispatcher
}

fn new_inner(config: &Config, ...) -> (Arc<Self>, impl FnOnce(&Arc<Self>)) {
    // ... setup ...
    let dispatcher = Arc::new(Self { dispatch_tx, ... });

    (dispatcher, |d| {
        let dispatcher = d.clone();
        hardy_async::spawn!(d.tasks, "dispatch_queue_consumer", async move {
            dispatcher.run_dispatch_queue(dispatch_rx).await
        });
    })
}
```

This pattern:
- Eliminates OnceLock OS synchronization overhead
- Avoids Arc::new_cyclic race conditions (task starting before Arc construction completes)
- Provides clear ownership and initialization order

### Cargo.toml Feature Flags (Phase 1)

The `bpa/Cargo.toml` now has proper no_std feature gating:

```toml
[features]
default = ["rfc9173", "tokio"]  # tokio implies std
rfc9173 = ["hardy-bpv7/rfc9173"]
tokio = ["std", "hardy-async/tokio"]  # tokio implies std
std = ["time/std", "hardy-bpv7/std", "hardy-eid-patterns/std", "serde?/std"]
serde = ["dep:serde", "hardy-bpv7/serde", "hardy-eid-patterns/serde", ...]

[dependencies]
hardy-async = { path = "../async", default-features = false }
hardy-bpv7 = { path = "../bpv7", default-features = false }
hardy-eid-patterns = { path = "../eid-patterns", default-features = false }
hashbrown = "0.16.1"
serde = { version = "1.0", default-features = false, features = ["derive", "rc", "alloc"], optional = true }
```

The `hardy-async/Cargo.toml` also has the `std` feature:

```toml
[features]
default = ["tokio"]
std = ["time/std"]
tokio = ["std", "dep:tokio", "dep:tokio-util"]  # tokio implies std

[dependencies]
time = { version = "0.3", default-features = false }
```

**Feature Chain:**
```
bpa default = ["rfc9173", "tokio"]
    └── tokio = ["std", "hardy-async/tokio"]
            ├── std = ["time/std", "hardy-bpv7/std", "hardy-eid-patterns/std", "serde?/std"]
            └── hardy-async/tokio = ["std", "dep:tokio", "dep:tokio-util"]
                    └── std = ["time/std"]
```

### Collections (Phase 4)

Collections are now properly abstracted in `lib.rs`:

```rust
#[cfg(feature = "std")]
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map, hash_map};

#[cfg(not(feature = "std"))]
use hashbrown::{HashMap, HashSet, hash_map};

#[cfg(not(feature = "std"))]
use alloc::collections::{BTreeMap, BTreeSet, btree_map};
```

### Arc/Weak Migration

All `Arc` and `Weak` imports now use `alloc::sync`:

```rust
use alloc::sync::{Arc, Weak};
```

This works for both std and no_std since `std::sync::Arc` is just a re-export of `alloc::sync::Arc`.

### std:: to core:: Migration

All applicable `std::` types have been migrated to `core::`:

| Before | After |
|--------|-------|
| `std::time::Duration` | `core::time::Duration` |
| `std::fmt::*` | `core::fmt::*` |
| `std::cmp::Ordering` | `core::cmp::Ordering` |
| `std::hash::Hasher` | `core::hash::Hasher` |
| `std::ops::Range` | `core::ops::Range` |
| `std::mem::take` | `core::mem::take` |
| `std::num::NonZeroUsize` | `core::num::NonZeroUsize` |
| `std::borrow::Cow` | `alloc::borrow::Cow` |
| `std::error::Error` | `core::error::Error` |
| `std::net::SocketAddr` | `core::net::SocketAddr` |

### Prelude Consistency

Prelude items use unqualified names where possible:
- `Default` - unqualified (in prelude)
- `Result` - unqualified except in type alias definitions (would be recursive)

Non-prelude items remain fully qualified:
- `core::fmt::Display`, `core::fmt::Formatter`
- `core::fmt::Debug` (kept qualified by preference)
- `core::hash::Hash`, `core::hash::Hasher`
- `core::cmp::Ordering`
- `core::error::Error`

### Crate-Level Setup (Phase 7 - Partial)

The `lib.rs` has the no_std foundation:

```rust
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
```

---

## Workspace Consistency

All workspace crates follow the same patterns:

| Crate | no_std Support | Notes |
|-------|----------------|-------|
| `hardy-cbor` | `#![no_std]` unconditionally | Fully no_std |
| `hardy-bpv7` | `#![cfg_attr(not(feature = "std"), no_std)]` | std feature gates HashMap/HashSet |
| `hardy-eid-patterns` | `#![cfg_attr(not(feature = "std"), no_std)]` | Optional std feature |
| `hardy-async` | Feature-gated via `std` | tokio implies std, time uses default-features = false |
| `hardy-bpa` | `#![cfg_attr(not(feature = "std"), no_std)]` | In progress, tokio implies std |

All crates use:
- `core::fmt::Display`, `core::fmt::Formatter` (not in prelude)
- `core::cmp::Ordering` (not in prelude)
- `core::error::Error` (Rust 1.81+)
- `alloc::sync::Arc` where applicable

---

## Remaining Work

### Unguarded std Dependencies

The following `std::` usages are NOT behind `#[cfg(feature = "std")]`:

| File | Usage | Fix Required |
|------|-------|--------------|
| `cla/peers.rs:15,22` | `std::sync::OnceLock` | Use `hardy_async::sync::spin::Once` |

**Resolved:**
- `rib/find.rs` - Now uses `foldhash::quality::RandomState::default().hash_one()` with `core::hash::BuildHasher`, which is no_std compatible
- `config.rs` - Now uses `hardy_async::available_parallelism()` which is feature-gated (returns 1 in no_std)

### Phase 2c: Channel Abstraction

The crate uses `flume` channels extensively for inter-task communication. Flume is std-only and cannot work on bare-metal no_std targets.

**Strategy:** Abstract channels through `hardy-async` with feature-gated implementations:
- For std: flume (current implementation)
- For Embassy: `embassy_sync::channel::Channel` (static allocation)

**Note:** Embassy channels require static allocation with compile-time capacity, which is a different model than flume's dynamic allocation. This will require careful API design.

**See:** `async/HARDY_ASYNC_PROPOSAL.md` Phase 2.6 for detailed channel abstraction design.

### Phase 2d: Metrics Abstraction

The crate uses the `metrics` crate for observability. Metrics is std-only.

**Strategy:** Abstract metrics through a trait with feature-gated implementations:
- For std: metrics crate (current implementation)
- For no_std: no-op implementation or compile-time feature to disable

**Alternative:** Consider `tinymetrics` for no_std environments if actual metrics collection is needed.

### Embedded Platform Support

**Completed:** Dependency cleanup for no_std compatibility.

All dependencies now have `default-features = false` where applicable, with proper `std` feature propagation:

```toml
std = [
    "time/std",
    "hardy-bpv7/std",
    "hardy-eid-patterns/std",
    "tracing/std",
    "rand/std",
    "thiserror/std",
    "futures/std",
    "serde?/std",
    "base64?/std",
    "foldhash/std",
]
```

**Embedded targets require:**

1. **Custom RNG backend**: The `rand` crate uses `getrandom` for entropy. Embedded targets must provide a custom backend via [getrandom's custom backend mechanism](https://docs.rs/getrandom/latest/getrandom/#custom-backend).

2. **64-bit atomics**: The `hardy-bpv7` crate uses `portable-atomic` for `AtomicU64`. On targets without native 64-bit atomics (e.g., thumbv6m), enable `hardy-bpv7/critical-section` and provide a critical-section implementation from your HAL.

See [hardy-bpv7 documentation](../../bpv7/docs/design.md#embedded-targets-and-custom-rng) for detailed instructions.

### Phase 3b: Embassy Backends (HIGH effort)

Once remaining phases are complete, Embassy backends need to be added to `hardy-async`:

| tokio | Embassy |
|-------|---------|
| `tokio::sync::Notify` | `embassy_sync::signal::Signal` |
| `tokio::sync::Semaphore` | `embassy_sync::semaphore::Semaphore` |
| `tokio::time::sleep` | `embassy_time::Timer::after` |
| `CancellationToken` | Custom or `embassy_sync::signal` |
| `flume` channels | `embassy_sync::channel::Channel` |

---

## Implementation Order

### Complete

1. Async runtime abstraction (bpa) - All async primitives via hardy-async
2. Phase 6: Time types - `core::time::Duration` for service API
3. Phase 1: Cargo.toml feature flags - Feature flags and dependency updates
4. Phase 4: Collections - hashbrown integration
5. Phase 5: Error handling - `core::error::Error` migration
6. Phase 7 (Partial): Crate-level setup - `#![no_std]` + alloc, Arc/Weak migration
7. Prelude consistency - Simplified qualifications
8. hardy-async `std` feature - Added with proper tokio implication
9. Phase 2a: Add `sync` module to `hardy-async` - Mutex, RwLock, spin::Mutex, spin::RwLock
10. Phase 2b: Update `bpa` imports - Uses `hardy_async::sync::spin::*` for hot paths
11. Dispatcher refactoring - Eliminated OnceLock via "return closure" pattern
12. Portable hasher in rib/find.rs - Uses `foldhash::quality::RandomState` with `core::hash::BuildHasher`
13. available_parallelism abstraction - Uses `hardy_async::available_parallelism()` with no_std fallback

### Remaining

1. **cfg-gate unguarded std usages** (LOW effort)
   - `std::sync::OnceLock` in cla/peers.rs → use `hardy_async::sync::spin::Once`

2. **Phase 2c**: Add `channel` module to `hardy-async` with flume re-exports (LOW effort)

3. **Phase 3b**: Embassy backends in `hardy-async` (HIGH effort)

---

## Testing Strategy

1. **Add CI job** for no_std build verification:
   ```bash
   cargo build --no-default-features --features alloc
   ```

2. **Test on embedded target**:
   ```bash
   cargo build --target thumbv7em-none-eabihf --no-default-features --features alloc
   ```

3. **Ensure std feature maintains full functionality**

---

## Open Questions

1. **Async runtime for no_std**: Embassy is the leading candidate. See HARDY_ASYNC_PROPOSAL.md Phase 3.

---

## Estimated Complexity

| Phase | Effort | Status | Notes |
|-------|--------|--------|-------|
| Runtime abstraction (bpa) | High | DONE | All async primitives via hardy-async |
| Phase 1 (Cargo.toml) | Low | DONE | Feature flags and dependency updates |
| hardy-async `std` feature | Low | DONE | tokio implies std, time default-features = false |
| Phase 4 (Collections) | Medium | DONE | hashbrown integration |
| Phase 5 (Errors) | Medium | DONE | core::error::Error migration |
| Phase 6 (Time) | Low | DONE | `core::time::Duration` for service API |
| Phase 7 (Crate setup) | Low | DONE | `#![no_std]` + alloc, Arc/Weak, std->core |
| Phase 2a (hardy-async sync) | Medium | DONE | Mutex, RwLock, spin::Mutex, spin::RwLock |
| Phase 2b (bpa sync imports) | Medium | DONE | Uses hardy_async::sync::spin for hot paths |
| Dispatcher refactoring | Medium | DONE | Eliminated OnceLock via closure pattern |
| Prelude consistency | Low | DONE | Simplified qualifications |
| Dependency cleanup | Low | DONE | default-features = false, std propagation |
| Embedded platform docs | Low | DONE | getrandom custom backend, portable-atomic |
| cfg-gate remaining std | Low | Pending | 1 unguarded usage remains (OnceLock) |
| Phase 2c (Channels) | Medium | Pending | flume abstraction in hardy-async |
| Phase 2d (Metrics) | Medium | Pending | metrics abstraction or removal |
| Phase 3b (Embassy backends) | High | Pending | Embassy integration for hardy-async |

**Overall**: The majority of the no_std groundwork is complete. The remaining blockers are:
1. cfg-gating 1 unguarded std usage (OnceLock in cla/peers.rs)
2. Channel abstraction through hardy-async (flume is std-only)
3. Metrics abstraction or removal (metrics is std-only)
4. Embassy backends (high effort, future work)
