# Rustdoc Style Guide

This guide defines conventions for rustdoc comments across Hardy crates. It is based on patterns already established in `hardy-cbor`, `hardy-bpv7`, `hardy-bpa`, and `hardy-async`.

## Crate-Level Documentation

Every library crate must have a crate-level doc comment at the top of `lib.rs`. Binary crates (`main.rs`) do not need one.

Use `/*! ... */` block comment style (not `//!` line comments) for readability.

**Caution:** `/* */` blocks cannot contain `*/` sequences — even inside backticks. If the comment text includes glob patterns like `*/S` or `dtn://**`, use `//` line comments instead.

The crate-level doc should include:

1. **One-line summary** — what the crate does
2. **Overview paragraph** — context, relationship to other crates, relevant standards
3. **Key modules or types** — brief list linking to the main entry points
4. **Feature flags** — if any, with descriptions

```rust
/*!
RFC 8949 compliant CBOR encoder/decoder for the Hardy DTN Router.

This crate provides low-level CBOR primitives used by [`hardy-bpv7`] for
bundle parsing and generation. It is `no_std` compatible with an allocator.

# Key Types

- [`decode`] — streaming CBOR decoder with byte-range tracking
- [`encode`] — canonical CBOR encoder

# Feature Flags

- `std` — enables `std::io` integration (default)
*/
```

## Public Item Documentation

Every public item (`pub fn`, `pub struct`, `pub enum`, `pub trait`, `pub type`) must have a doc comment. Items that are not part of the published API do not need rustdoc:

- Private items (`fn`, `struct`, etc. without `pub`)
- `pub(crate)` items — visible within the crate but not to consumers
- `pub` items inside private modules (`mod foo`, not `pub mod foo`)

Use `//` comments on these if the logic needs explaining. Confirm that public types are actually exposed via a public module path before adding rustdoc.

### One-Liner Items

Simple items use a single `///` line:

```rust
/// The bundle's creation timestamp.
pub timestamp: CreationTimestamp,
```

### Functions and Methods

Document what the function does, not how. Include parameters only when their purpose is not obvious from name and type.

```rust
/// Registers a CLA with the BPA under the given name.
///
/// Returns an error if a CLA with the same name is already registered.
pub async fn register_cla(&self, name: String, ...) -> Result<()> {
```

### Complex Items

For types or functions with non-trivial behaviour, use structured sections:

```rust
/// A cancellable pool of async tasks with panic propagation.
///
/// Tasks spawned on the pool are cancelled when the pool is dropped.
/// If any task panics, the panic is propagated to the next `join()` call.
///
/// # Examples
///
/// ```rust,no_run
/// let pool = TaskPool::new();
/// pool.spawn(async { /* ... */ });
/// pool.join().await;
/// ```
///
/// # Panics
///
/// `join()` panics if any spawned task panicked.
```

## Sections

Use only these standard sections, in this order:

| Section | When to use |
|---------|-------------|
| `# Examples` | Complex APIs where usage is not obvious |
| `# Panics` | When the function can panic in normal use |
| `# Errors` | When returning `Result` — describe error conditions |
| `# Safety` | `unsafe` functions only |

Do not add sections that repeat the summary. If a function's error conditions are obvious from the `Result` type, omit `# Errors`.

## Examples

- Use `rust,no_run` for examples that need a runtime or network
- Use `rust,ignore` only for incomplete snippets
- Keep examples minimal — show the API call, not the setup
- Examples must compile (CI will check this)

## RFC and Standard References

When a type or function implements a specific RFC section, reference it:

```rust
/// Parses the primary block fields defined in [RFC 9171 Section 4.3.1].
///
/// [RFC 9171 Section 4.3.1]: https://datatracker.ietf.org/doc/html/rfc9171#section-4.3.1
```

Use reference-style links at the bottom of the doc comment. Do not inline URLs.

## Cross-References

Link to other types and modules using rustdoc syntax:

```rust
/// Returns the [`Bundle`] parsed from the given bytes.
///
/// See [`BundleBuilder`] for constructing bundles programmatically.
```

Use full paths when referencing items in other crates:

```rust
/// Uses [`hardy_cbor::decode`] for CBOR parsing.
```

## Configuration Structs

All `Config` structs with `serde` derive must document every field:

```rust
/// BPA server configuration.
pub struct Config {
    /// gRPC listen address. Default: `[::]:50051`.
    pub address: String,

    /// Maximum bundle size eligible for LRU caching, in bytes.
    pub max_cached_bundle_size: NonZeroUsize,
}
```

Include the default value in the doc comment when there is one.

## Traits

Trait documentation should describe the contract, not the implementation. Include:

1. What implementors must provide
2. What callers can expect
3. Lifecycle (if registration/unregistration is involved)

```rust
/// A convergence layer adapter that can send and receive bundles.
///
/// Implementors register with a [`Bpa`] via [`Bpa::register_cla`] and
/// receive a [`ClaSink`] for forwarding received bundles. Dropping the
/// sink unregisters the CLA.
pub trait Cla: Send + Sync + 'static {
```

## Trait Implementations

Trait impl methods do not need `///` doc comments unless the implementation behaviour is surprising or deviates from the trait documentation. A normal `//` comment explaining the approach is useful:

```rust
impl BundleStorage for Storage {
    // Atomic write: temp file + rename + dir fsync
    async fn store(&self, id: &str, data: Bytes) -> Result<()> {
```

## What Not To Document

- Re-exports (document at the source)
- `impl` blocks for derived traits (`Debug`, `Clone`, etc.)
- Trait impl methods (unless behaviour is surprising — use `//` comments instead)
- Test modules and test helper functions
- Items behind `#[doc(hidden)]`

## Checking Documentation

```bash
# Check for missing docs on public items
cargo doc --no-deps --package <crate> 2>&1 | grep "warning"

# Build docs for the whole workspace
cargo doc --no-deps --workspace

# Verify examples compile
cargo test --doc --package <crate>
```

## Correctness Pass

After adding or updating doc comments, always perform a correctness pass against the actual code. Documentation that describes wrong behaviour is worse than no documentation.

Check for:

1. **Signature mismatches** — do parameter types and return types in docs match the code?
2. **Stale defaults** — do documented default values match the `Default` impl?
3. **Renamed or removed items** — do cross-references point to types/methods that still exist?
4. **Example code** — would the examples compile against the current API?
5. **Feature flag references** — are conditional compilation features still valid?
6. **Behavioural claims** — does the code actually do what the doc says it does?

This pass should be performed whenever docs are written or code is refactored.
