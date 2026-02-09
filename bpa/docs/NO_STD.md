# Making `bpa` no_std Compatible

**Last Updated:** 2026-02-09

This document outlines the work required to make the `bpa` package `no_std` compatible as an optional feature.

**Related Document:** See `/workspace/async/HARDY_ASYNC_PROPOSAL.md` for the runtime abstraction strategy and current migration status.

**Scope:** This document covers only the `bpa` crate. Higher-level packages (bpa-server, tcpclv4, proto, etc.) are out of scope for no_std and remain tokio-dependent.

**Target:** `no_std` + `alloc` is the only viable path forward. Pure `no_std` (no heap) is not practical for a BPA implementation due to dynamic data structures (bundles, routing tables, registries). Custom allocators in Rust are not yet mainstream, and embedded platforms like Embassy do not yet have stable support for them. Once custom allocators mature, more fine-grained memory control may become possible.

## Current State Summary

| Category | Status | Severity |
|----------|--------|----------|
| Async Runtime (`hardy-async`) | **bpa fully abstracted** - zero direct tokio deps | ✅ DONE |
| Synchronization (`std::sync`) | 130+ Arc uses, 6 RwLock, 5 Mutex | CRITICAL |
| Collections | HashMap (6), BTreeMap/BTreeSet (multiple) | HIGH |
| Error handling | `Box<dyn std::error::Error>` | MODERATE |
| Time | `std::time::Duration` (3 uses) | LOW |
| Allocator | Requires `alloc` (Vec, String, Arc, Box) | EXPECTED |

### What's Already Done

The `bpa` crate now has **zero direct tokio dependencies**. All async primitives are accessed through `hardy-async` abstractions:

| Primitive | hardy-async Abstraction | Status |
|-----------|------------------------|--------|
| `tokio::spawn` | `hardy_async::spawn!` macro | ✅ Migrated |
| `tokio::task::JoinHandle` | `hardy_async::JoinHandle` | ✅ Migrated |
| `tokio::task::JoinSet` + `Semaphore` | `hardy_async::BoundedTaskPool` | ✅ Migrated |
| `tokio::sync::Notify` | `hardy_async::Notify` | ✅ Migrated |
| `tokio::time::sleep` | `hardy_async::time::sleep` | ✅ Migrated |
| `tokio::select!` | `futures::select_biased!` | ✅ Migrated |
| `tokio_util::CancellationToken` | `hardy_async::CancellationToken` | ✅ Migrated |

This means bpa is ready for Embassy support once hardy-async has Embassy backends.

## Workspace Precedent

Several workspace crates already support `no_std`:
- `hardy-cbor` - fully `no_std` compatible
- `hardy-bpv7` - has `no_std` with conditional `std` feature
- `hardy-eid-patterns` - has `no_std` with optional `std` feature

These provide patterns to follow.

---

## Phase 1: Cargo.toml Feature Gating

### Actions

1. **Add feature flags**:
   ```toml
   [features]
   default = ["std"]
   std = [
       "hardy-bpv7/std",
       "hardy-eid-patterns/std",
       "hardy-async/std",
       "time/std",
       "thiserror/std",
   ]
   alloc = []  # For no_std + alloc
   ```

2. **Update dependencies with `default-features = false`**:
   ```toml
   [dependencies]
   hardy-bpv7 = { path = "../bpv7", default-features = false }
   hardy-eid-patterns = { path = "../eid-patterns", default-features = false }
   time = { version = "0.3", default-features = false }
   thiserror = { version = "2", default-features = false }
   ```

3. **Add no_std alternatives**:
   ```toml
   hashbrown = "0.14"  # HashMap replacement (already used in bpv7)
   # Note: spin/once_cell are handled via hardy-async (see Phase 2)
   ```

---

## Phase 2: Synchronization Abstraction via `hardy-async` (CRITICAL)

This is the largest blocker. The crate uses `std::sync::{Mutex, RwLock, Arc, Weak, OnceLock}` extensively.

**Strategy**: Wrap `Mutex` and `RwLock` in `hardy-async` alongside existing abstractions (`Notify`, `CancellationToken`, `JoinHandle`). This centralizes sync primitives and enables target-appropriate implementations.

### Files Requiring Changes in `bpa`

| File | Primitives Used |
|------|-----------------|
| `keys/registry.rs` | `RwLock<HashMap<...>>` |
| `cla/peers.rs` | `RwLock`, `Weak`, `OnceLock` |
| `cla/registry.rs` | `Mutex`, `RwLock`, `Weak`, `HashMap` |
| `rib/mod.rs` | `RwLock`, `HashMap`, `BTreeMap`, `BTreeSet` |
| `storage/mod.rs` | `Mutex`, `BTreeSet` |
| `storage/bundle_mem.rs` | `Mutex` |
| `storage/channel.rs` | `Mutex` |
| `services/registry.rs` | `HashMap`, `RwLock`, `Weak` |

### Actions in `hardy-async`

1. **Add sync module** (`async/src/sync.rs`):
   ```rust
   //! Synchronization primitives with target-appropriate implementations.

   extern crate alloc;

   // Arc/Weak: always from alloc (std re-exports these anyway)
   pub use alloc::sync::{Arc, Weak};

   // Mutex/RwLock: std for std targets, spin for no_std
   #[cfg(feature = "std")]
   pub use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

   #[cfg(not(feature = "std"))]
   pub use spin::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

   // OnceLock: std 1.70+ or once_cell for no_std
   #[cfg(feature = "std")]
   pub use std::sync::OnceLock;
   #[cfg(not(feature = "std"))]
   pub use once_cell::race::OnceBox as OnceLock;  // Or spin::Once
   ```

2. **Update `async/Cargo.toml`**:
   ```toml
   [features]
   default = ["std", "tokio"]
   std = []
   tokio = ["std", "dep:tokio", "dep:tokio-util"]
   tracing = ["dep:tracing"]

   [dependencies]
   spin = { version = "0.9", optional = true }
   once_cell = { version = "1", default-features = false, optional = true }

   # Enable spin/once_cell for no_std
   [target.'cfg(not(feature = "std"))'.dependencies]
   spin = "0.9"
   once_cell = { version = "1", default-features = false, features = ["race"] }
   ```

3. **Export from `async/src/lib.rs`**:
   ```rust
   pub mod sync;
   ```

### Actions in `bpa`

1. **Update imports** across all affected files:
   ```rust
   // Before
   use std::sync::{Arc, Mutex, RwLock};

   // After
   use hardy_async::sync::{Arc, Mutex, RwLock};
   ```

2. **Handle `OnceLock`** in `cla/peers.rs`:
   ```rust
   use hardy_async::sync::OnceLock;
   ```

3. **Update all 130+ Arc usages** to use `hardy_async::sync::Arc`

### API Considerations

The `spin` crate's `Mutex` and `RwLock` have slightly different APIs:

| Operation | `std::sync` | `spin` |
|-----------|-------------|--------|
| Lock | `.lock().unwrap()` | `.lock()` (infallible) |
| Try lock | `.try_lock()` → `Result` | `.try_lock()` → `Option` |
| Poisoning | Yes | No |

**Recommendation**: Create thin wrappers in `hardy-async` that normalize the API, or update `bpa` call sites to handle both patterns via a trait.

---

## Phase 3: Runtime Abstraction (IN PROGRESS)

**Status:** bpa is fully abstracted; Embassy backends are the remaining work.

The `bpa` crate now has zero direct tokio dependencies - all async primitives go through `hardy-async`. This phase is about adding Embassy backends to `hardy-async`.

### Current `hardy-async` Abstractions

| Abstraction | Current Implementation | no_std Status |
|-------------|------------------------|---------------|
| `TaskPool` | tokio TaskTracker | ✅ bpa migrated, needs Embassy backend |
| `BoundedTaskPool` | TaskPool + tokio Semaphore | ✅ bpa migrated, needs Embassy backend |
| `CancellationToken` | tokio_util | ✅ bpa migrated, needs Embassy backend |
| `JoinHandle` | tokio | ✅ bpa migrated, needs Embassy backend |
| `Notify` | tokio::sync::Notify | ✅ bpa migrated, needs Embassy backend |
| `sleep()` | tokio::time::sleep | ✅ bpa migrated, needs Embassy backend |

### Actions

1. **Add `std` feature flag** to `hardy-async`:
   - `tokio` feature implies `std`
   - New features: `embassy` for embedded async runtime

2. **Feature-gate runtime components**:
   ```rust
   #[cfg(feature = "tokio")]
   mod tokio_impl;

   #[cfg(feature = "embassy")]
   mod embassy_impl;

   // Re-export based on active feature
   #[cfg(feature = "tokio")]
   pub use tokio_impl::*;

   #[cfg(feature = "embassy")]
   pub use embassy_impl::*;
   ```

3. **Embassy equivalents** (for no_std async):

   | tokio | Embassy |
   |-------|---------|
   | `tokio::sync::Notify` | `embassy_sync::signal::Signal` |
   | `tokio::sync::Semaphore` | `embassy_sync::semaphore::Semaphore` |
   | `tokio::time::sleep` | `embassy_time::Timer::after` |
   | `CancellationToken` | Custom or `embassy_sync::signal` |

4. **Alternative**: For simpler no_std support, provide sync-only API (no async runtime)

---

## Phase 4: Collections Replacement

### HashMap Usage (6 files)

| File | Usage |
|------|-------|
| `keys/registry.rs` | `HashMap<KeyId, ...>` |
| `cla/registry.rs` | `HashMap<String, ...>` |
| `cla/mod.rs` | `HashMap<...>` |
| `rib/mod.rs` | `HashMap<...>` |
| `policy/mod.rs` | `HashMap<...>` |
| `services/registry.rs` | `HashMap<...>` |

### Actions

1. **Add hashbrown**:
   ```toml
   hashbrown = { version = "0.14", default-features = false }
   ```

2. **Create collections abstraction** (`src/collections.rs`):
   ```rust
   #[cfg(feature = "std")]
   pub use std::collections::{HashMap, HashSet};

   #[cfg(not(feature = "std"))]
   pub use hashbrown::{HashMap, HashSet};

   // BTreeMap/BTreeSet are in alloc
   #[cfg(feature = "std")]
   pub use std::collections::{BTreeMap, BTreeSet};

   #[cfg(not(feature = "std"))]
   pub use alloc::collections::{BTreeMap, BTreeSet};
   ```

3. **Update all imports** to use `crate::collections::*`

---

## Phase 5: Error Handling

### Current Pattern

```rust
// bpa.rs, services/mod.rs, cla/mod.rs, storage/mod.rs
Box<dyn std::error::Error + Send + Sync>
```

### Actions

1. **Use `core::error::Error`** (stabilized in Rust 1.81):
   - `storage/mod.rs` already uses `core::error::Error`
   - Update other files to match

2. **Feature-gate thiserror**:
   ```rust
   #[cfg(feature = "std")]
   use thiserror::Error;

   #[cfg(not(feature = "std"))]
   // Use manual impl or thiserror with no_std feature
   ```

3. **Consider replacing dyn errors** with concrete enum types for no_std

---

## Phase 6: Time Types

### Usage Locations

| File | Line | Usage |
|------|------|-------|
| `services/mod.rs` | - | `std::time::Duration` lifetime param |
| `services/registry.rs` | - | `std::time::Duration` lifetime param |
| `dispatcher/local.rs` | 10 | `std::time::Duration` lifetime param |

### Actions

1. **Use `core::time::Duration`** (available since Rust 1.25):
   ```rust
   use core::time::Duration;
   ```

2. **Ensure `time` crate is configured**:
   ```toml
   time = { version = "0.3", default-features = false }
   ```

---

## Phase 7: Crate-Level Changes

### Actions

1. **Add no_std attribute** to `lib.rs`:
   ```rust
   #![cfg_attr(not(feature = "std"), no_std)]

   #[cfg(not(feature = "std"))]
   extern crate alloc;
   ```

2. **Replace std prelude imports**:
   ```rust
   #[cfg(not(feature = "std"))]
   use alloc::{boxed::Box, string::String, vec::Vec, vec};
   ```

3. **Remove/gate any remaining std usage**:
   - `std::fmt::*` → `core::fmt::*`
   - `std::hash::*` → `core::hash::*`
   - `std::cmp::*` → `core::cmp::*`
   - `std::num::NonZeroUsize` → `core::num::NonZeroUsize`

---

## Implementation Order

### Already Complete

- ✅ **Runtime abstraction in bpa** - All async primitives abstracted via hardy-async
  - TaskPool, BoundedTaskPool, spawn! macro
  - Notify, sleep, CancellationToken, JoinHandle
  - select! → select_biased! migration

### Remaining Work

1. **Phase 6**: Time types (LOW effort) - `std::time::Duration` → `core::time::Duration`
2. **Phase 5**: Error handling (MEDIUM effort) - `core::error::Error` migration
3. **Phase 4**: Collections (MEDIUM effort) - hashbrown integration
4. **Phase 2a**: Add `sync` module to `hardy-async` (MEDIUM effort) - Arc, Mutex, RwLock abstraction
5. **Phase 2b**: Update `bpa` imports to use `hardy_async::sync` (MEDIUM effort)
6. **Phase 1**: Cargo.toml feature flags (LOW effort) - after dependencies are ready
7. **Phase 3b**: Embassy backends in `hardy-async` (HIGH effort) - runtime alternatives
8. **Phase 7**: Crate-level finalization - `#![no_std]` + alloc setup

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
2. **Spinlocks acceptable?**: `spin` crate works but has performance implications for contended locks
3. **API normalization**: Should `hardy-async::sync` provide wrapper types that unify `std` and `spin` APIs (e.g., handling poisoning differences)?
4. ~~**Bounded collections**: Should `heapless` be used for truly no-alloc scenarios?~~ **Resolved:** Not needed; `no_std + alloc` is the target.
5. ~~**Scope**: Is `no_std + alloc` sufficient, or is pure `no_std` (no heap) required?~~ **Resolved:** `no_std + alloc` is the only viable path. Pure no_std is impractical for a BPA, and custom allocators are not yet mainstream in embedded Rust.

---

## Estimated Complexity

| Phase | Effort | Status | Notes |
|-------|--------|--------|-------|
| Runtime abstraction (bpa) | High | ✅ **DONE** | All async primitives via hardy-async |
| Phase 1 (Cargo.toml) | Low | Pending | Feature flags and dependency updates |
| Phase 2a (hardy-async sync module) | Medium | Pending | New `sync.rs` module with feature-gated exports |
| Phase 2b (bpa sync imports) | Medium | Pending | Update 130+ Arc uses, 11 Mutex/RwLock uses |
| Phase 3b (Embassy backends) | High | Pending | Embassy integration for hardy-async |
| Phase 4 (Collections) | Medium | Pending | hashbrown integration, 6 files |
| Phase 5 (Errors) | Medium | Pending | core::error::Error migration |
| Phase 6 (Time) | Low | Pending | core::time::Duration swap |
| Phase 7 (Finalization) | Low | Pending | #![no_std] + alloc setup |

**Overall**: The critical async runtime abstraction for bpa is complete. Centralizing sync primitives in `hardy-async` is the next major step, followed by Embassy backends. The remaining work is mechanical but requires careful API compatibility.
