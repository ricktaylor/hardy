# Storage Test Harness Design

General-purpose test harness for verifying storage backend implementations against the `MetadataStorage` and `BundleStorage` trait contracts.

## Related Documents

- **[Storage Integration Test Plan](storage_integration_test_plan.md)** (`PLAN-STORE-01`): Defines the test suites (A-D) that the harness executes
- **[Storage Subsystem Design](storage_subsystem_design.md)**: Architecture of the dual storage model
- **[SQLite Test Plan](../../sqlite-storage/docs/test_plan.md)** (`PLAN-SQLITE-01`): SQLite-specific tests and harness invocation
- **[Localdisk Test Plan](../../localdisk-storage/docs/test_plan.md)** (`PLAN-LD-01`): Localdisk-specific tests and harness invocation
- **[Test Strategy](../../docs/test_strategy.md)**: Overall testing pyramid; places the harness at Level 2 (Component Testing)

## Motivation

The BPA defines two storage traits (`MetadataStorage` and `BundleStorage`) that decouple the dispatch engine from any specific persistence technology. Today there are three backend implementations:

| Trait | Backend | Crate |
|-------|---------|-------|
| `MetadataStorage` | SQLite | `sqlite-storage` |
| `MetadataStorage` | In-memory (LRU) | `hardy-bpa` (`storage::metadata_mem`) |
| `BundleStorage` | Local filesystem | `localdisk-storage` |
| `BundleStorage` | In-memory (LRU) | `hardy-bpa` (`storage::bundle_mem`) |

Additional backends are planned (PostgreSQL for metadata per REQ-8, S3 for bundles per REQ-9). The fuzz test overhaul removes the incidental storage coverage that came from full-pipeline fuzzing. A dedicated harness ensures that every backend, current and future, is verified against the same canonical test suite without duplicating test logic.

## Design Goals

1. **Write once, run everywhere.** A single body of test logic covers all backends. Adding a new backend requires only a factory function, not new test code.
2. **Trait-level testing.** Tests exercise the public trait API and assert the behavioural contract documented in `PLAN-STORE-01`. They do not test backend internals (SQL queries, filesystem layout, etc.) -- those belong in per-crate unit tests.
3. **Minimal fixture overhead.** Each test constructs its own state. No shared mutable fixtures, no test ordering dependencies.
4. **Parallel safety.** Tests for a single backend use isolated instances (temp directories, in-memory stores) and can run concurrently via `cargo test`.
5. **Easy CI integration.** The harness runs as standard `cargo test` targets with no external service dependencies for the default backends.

## Architecture Overview

```
tests/storage_harness/
  mod.rs              -- re-exports, backend registration
  metadata_suite.rs   -- Suite A, B, C  (MetadataStorage tests)
  bundle_suite.rs     -- Suite D        (BundleStorage tests)
  fixtures.rs         -- Bundle/metadata factory helpers

tests/storage_harness.rs  -- #[test] entry point (calls suites for each backend)
```

The harness is a library of async test functions parameterised by a factory closure. The entry-point file instantiates each backend and feeds it into the shared suites.

### Test Registration Pattern

Each suite is a set of standalone async functions that accept a factory closure rather than a pre-built storage instance. This lets each test create a fresh, isolated backend:

```rust
/// Factory signature for MetadataStorage backends.
type MetaFactory = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Arc<dyn MetadataStorage>>>> + Send + Sync>;

/// Factory signature for BundleStorage backends.
type BlobFactory = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Arc<dyn BundleStorage>>>> + Send + Sync>;
```

A backend registers itself by providing a factory, for example:

```rust
// In tests/storage_harness.rs
fn sqlite_meta_factory() -> MetaFactory {
    Box::new(|| Box::pin(async {
        let dir = tempfile::tempdir().unwrap();
        sqlite_storage::new(&sqlite_storage::Config {
            directory: dir.path().into(),
            ..Default::default()
        }) as Arc<dyn MetadataStorage>
    }))
}
```

Each `#[tokio::test]` then calls a suite function:

```rust
#[tokio::test]
async fn sqlite_meta_insert_and_get() {
    metadata_suite::meta_01_insert_and_get(sqlite_meta_factory()).await;
}
```

This keeps test output clean -- each test ID maps to a named `#[test]` function, and failures identify both the suite case and the backend.

### Alternative: Macro Registration

To reduce the boilerplate of writing one `#[tokio::test]` per (backend x test-case), a declarative macro can stamp out the cartesian product:

```rust
macro_rules! storage_tests {
    ($backend:ident, $factory:expr, [$($test:ident),* $(,)?]) => {
        $(
            #[tokio::test]
            async fn $test() {
                let factory = $factory;
                metadata_suite::$test(factory).await;
            }
        )*
    };
}

mod sqlite {
    use super::*;
    storage_tests!(sqlite, sqlite_meta_factory(), [
        meta_01_insert_and_get,
        meta_02_duplicate_insert,
        meta_03_update_replace,
        // ...
    ]);
}
```

This is the recommended approach. It keeps the registration DRY while still producing individually-named test functions that `cargo test` can filter and report on.

## Test Suite Mapping

The harness implements every test case from `PLAN-STORE-01`. The table below maps plan IDs to harness functions:

### MetadataStorage (Suites A-C)

| Plan ID | Harness Function | Contract Verified |
|---------|-----------------|-------------------|
| META-01 | `meta_01_insert_and_get` | Insert returns `true`; get returns matching bundle |
| META-02 | `meta_02_duplicate_insert` | Second insert of same ID returns `false` |
| META-03 | `meta_03_update_replace` | Replace persists status change (`Waiting` -> `Dispatching`) |
| META-04 | `meta_04_tombstone` | Tombstoned bundle invisible to get; re-insert blocked |
| META-05 | `meta_05_confirm_exists` | Returns metadata for existing bundles, `None` for missing |
| META-06 | `meta_06_poll_waiting_fifo` | Waiting bundles returned in received-time order |
| META-07 | `meta_07_poll_expiry` | Bundles returned in expiry-time order; `New` status excluded; limit respected |
| META-08 | `meta_08_poll_pending_limit` | Respects limit parameter; FIFO within status |
| META-09 | `meta_09_poll_pending_exact_match` | Filters on all enum variant fields (`peer` and `queue` both discriminate) |
| META-10 | `meta_10_poll_adu_fragments` | Fragments with matching `AduFragment { source, timestamp }` returned in `fragment_info.offset` order |
| META-11 | `meta_11_reset_peer_queue` | Returns `true`; targeted peer's `ForwardPending` bundles become `Waiting`; other peers unchanged |
| META-12 | `meta_12_recovery` | `start_recovery()` completes without error |
| META-13 | `meta_13_remove_unconfirmed` | Unconfirmed bundles sent to channel; operation completes |

### BundleStorage (Suite D)

| Plan ID | Harness Function | Contract Verified |
|---------|-----------------|-------------------|
| BLOB-01 | `blob_01_save_and_load` | Round-trip: saved bytes match loaded bytes |
| BLOB-02 | `blob_02_delete` | Deleted bundle returns `None` on load |
| BLOB-03 | `blob_03_missing_load` | Load of non-existent name returns `Ok(None)` |
| BLOB-04 | `blob_04_recovery_scan` | `recover()` emits `(storage_name, timestamp)` entries for all stored bundles |

## Fixture Helpers

Tests need to construct valid `Bundle` and `BundleMetadata` values to exercise the storage traits. A `fixtures` module provides builder functions that produce minimal valid instances with controllable fields:

```rust
/// Create a bundle with a unique random ID.
pub fn random_bundle() -> bundle::Bundle { ... }

/// Create a bundle with a specific status and received_at timestamp.
pub fn bundle_with_status(status: BundleStatus, received_at: OffsetDateTime) -> bundle::Bundle { ... }

/// Create a bundle with a specific expiry time.
pub fn bundle_with_expiry(expiry: OffsetDateTime) -> bundle::Bundle { ... }

/// Create a bundle with a specific fragment offset.
pub fn bundle_with_fragment(status: BundleStatus, offset: u64) -> bundle::Bundle { ... }

/// Generate random bundle payload data of a given size.
pub fn random_payload(size: usize) -> Bytes { ... }
```

These helpers encapsulate the complexity of constructing valid BPv7 bundle IDs (source EID, creation timestamp, sequence number, fragment info) so that test functions remain focused on the storage contract being verified.

## Backend Lifecycle

Each backend may need setup and teardown. The factory closure is responsible for both:

- **In-memory backends**: No setup needed. The factory calls `metadata_mem::new()` or `bundle_mem::new()` with a default config. No teardown.
- **SQLite**: The factory creates a `tempfile::TempDir`, passes the path to `sqlite_storage::new()`, and holds the `TempDir` handle so the directory lives for the duration of the test. Dropping the handle triggers cleanup.
- **Localdisk**: Same pattern as SQLite -- a temp directory scoped to the test.
- **Future backends (PostgreSQL, S3)**: The factory connects to a test-scoped database or bucket. These backends require external services and should be gated behind feature flags or environment variables (e.g., `#[cfg(feature = "test-postgres")]`) so they don't block CI for contributors without those services.

The factory-per-test pattern means no test sees state from another test, eliminating ordering bugs.

## CI Integration

### Default Run (No External Services)

```
cargo test --test storage_harness
```

This runs all suites against the in-memory and local (SQLite, localdisk) backends. These require no external infrastructure and should run in every CI pipeline.

### Extended Run (External Services)

```
cargo test --test storage_harness --features test-postgres,test-s3
```

Backends requiring external services are gated behind Cargo feature flags. CI jobs that provision those services (e.g., a PostgreSQL container, LocalStack for S3) enable the corresponding features.

### Per-Backend Filtering

Since every test function is named `<backend>_<test_id>`, standard `cargo test` filtering works:

```
cargo test --test storage_harness sqlite       # SQLite only
cargo test --test storage_harness meta_04      # Tombstone test across all backends
cargo test --test storage_harness blob         # All BundleStorage tests
```

## Adding a New Backend

To verify a new storage backend with the harness:

1. **Write a factory function** that constructs an instance of the new backend behind `Arc<dyn MetadataStorage>` or `Arc<dyn BundleStorage>`.
2. **Add a module** in the test entry point using the `storage_tests!` macro, listing all applicable suite functions.
3. **Gate on a feature flag** if the backend requires external services.
4. **Write a component-level test plan** for the new crate (following the pattern of `PLAN-SQLITE-01` / `PLAN-LD-01`) that references `PLAN-STORE-01` as its parent and adds any implementation-specific unit tests.

No changes to the suite functions are required. If the trait API changes, the harness is the single place where behavioural expectations are updated.

## Relationship to Fuzz Testing

The BPA fuzz redesign (`FUZZ-BPA-01`) focuses on pipeline logic and uses the in-memory backends as non-injectable fixtures. It intentionally does not vary the storage backend, because backend correctness is a separate concern from pipeline robustness.

The storage harness fills the gap: it verifies that every backend faithfully implements the trait contract, so the fuzz tests can trust the in-memory backend as a correct reference implementation.

## Testing

- [Storage Integration Test Plan](storage_integration_test_plan.md) (`PLAN-STORE-01`) -- defines the canonical test cases this harness implements
