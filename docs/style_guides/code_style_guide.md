# Rust Code Style Guide

This guide defines the general Rust coding conventions used across Hardy crates. It complements the more specialised guides — [rustdoc](rustdoc_style_guide.md) for doc comments, [design docs](design_doc_style_guide.md), [READMEs](readme_style_guide.md), and [coverage reports](coverage_report_style_guide.md) — and is based on the patterns already established in `hardy-cbor`, `hardy-bpv7`, `hardy-bpa`, and `hardy-async`.

The conventions here are what reviewers expect and what keeps a change reading like the code around it. When in doubt, match the surrounding module and write idiomatic Rust.

## Applying These Conventions

These conventions have converged over time, so not all existing code complies with every rule here. That is expected. **Do not make style-only sweeps across untouched files** — they create large, hard-to-review diffs and churn history for no functional gain.

Bring a file into line with this guide only when you are already modifying it for another reason, and keep the tidy-ups proportionate to the change so the substantive diff stays easy to review.

## Idiomatic Rust First

Default to idiomatic, community-standard Rust. This guide records Hardy's *project-specific* conventions and the few places we deviate from the norm — it is not a complete style manual. For everything it does not cover, follow the canonical references:

- [The Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) — the checklist for predictable, idiomatic public APIs (naming, trait implementations, interoperability).
- [The Rust Style Guide](https://doc.rust-lang.org/style-guide/) — the formatting and layout conventions that `rustfmt` implements.
- [The Rust Book](https://doc.rust-lang.org/book/) and [Rust by Example](https://doc.rust-lang.org/rust-by-example/) — idioms and patterns.

Clippy is the practical enforcer of most of these idioms; under `-D warnings` (see [Linting](#linting)) its suggestions are not optional. Prefer the idioms the compiler and Clippy steer you toward:

- Express flow with iterators and `Option`/`Result` combinators (`map`, `and_then`, `ok_or`, `?`) rather than manual index loops or nested matches, where it reads more clearly.
- Implement the standard conversion and formatting traits — `From` / `TryFrom`, `Display`, `FromStr`, `Default`, `Iterator` — instead of bespoke `to_x` / `from_x` methods, so types compose with the ecosystem. (EIDs parse via `FromStr`; bundles are constructed with a builder; dropping a `Sink` unregisters via `Drop` — RAII.)
- Accept borrowed types in function signatures (`&str`, `&[T]`, `impl AsRef<…>`) and return owned values; do not take `String` / `Vec<T>` by value just to read from it.
- Use the newtype pattern to give wire and domain values distinct types rather than threading raw integers around.
- Avoid needless `clone()` and intermediate allocations on hot paths — borrow, or move, instead.

## Toolchain and Edition

- The workspace targets the **stable** toolchain (`rust-toolchain.toml` pins `channel = "stable"` with `rustfmt` and `clippy`).
- The edition and MSRV are defined once in the workspace [`Cargo.toml`](../Cargo.toml) (`[workspace.package]` `edition` and `rust-version`) and inherited per crate via `edition.workspace = true` / `rust-version.workspace = true` — never hard-code them in a crate's `Cargo.toml`. Check that file for the current values.
- Do not use nightly-only features. If a feature is not available on the stable toolchain at the workspace MSRV, it is not available here.

## Formatting

Formatting is decided by `rustfmt` with default settings — there is no `rustfmt.toml`. Run `cargo fmt` (editors in the repo format on save). Do not hand-format code to deviate from what `rustfmt` produces; CI runs `cargo fmt --check` and a mismatch fails the build.

## Linting

Clippy is a hard gate. CI runs:

```bash
cargo clippy --locked --all-targets --all-features -- -D warnings
```

Every warning is an error, across all targets and all features. Fix the lint rather than suppressing it. If a lint genuinely does not apply, use a narrowly-scoped `#[allow(...)]` on the specific item (not a crate-wide allow) with a `//` comment explaining why.

There is no `[workspace.lints]` table; lint levels come from clippy's defaults under `-D warnings`. Do not add per-crate lint configuration without discussing it first.

## Naming

Follow standard Rust naming (Clippy enforces most of it):

- `snake_case` for functions, methods, variables, modules, and crate features.
- `UpperCamelCase` for types, traits, and enum variants.
- `SCREAMING_SNAKE_CASE` for constants and statics.
- Crate names are `hardy-<name>` (hyphenated) on disk and `hardy_<name>` when imported.
- Prefer descriptive names over abbreviations, but keep domain terms from the RFCs intact (`eid`, `cla`, `bpsec`, `bib`, `bcb`) — they are the vocabulary of the codebase.

## Imports and `use` Blocks

Organise `use` statements into up to three blocks, each separated by a single blank line, in this order:

1. The standard library — `std::…`, `core::…`, and `alloc::…` all share this one block (`no_std` crates use `core`/`alloc` in place of `std`).
2. Third-party crates — `<external_crate>::…`.
3. Local imports — `crate::…`, `super::…`, and `self::…` share this one block.

This is the order of the (currently unstable) rustfmt option `group_imports = "StdExternalCrate"`: writing it by hand now means that if the option stabilises and we enable it, `cargo fmt` reproduces the layout with no churn. Omit any block that has no imports (hence "up to three") — never leave an empty block. `rustfmt` sorts within each block and preserves the blank-line separation, but under its default settings it does not reorder or merge the blocks themselves, so the order above is a manual discipline.

```rust
use core::num::NonZeroUsize;

use thiserror::Error;

use crate::{bundle::Bundle, eid::Eid};
```

**Collapse imports that share a leading path** into a single nested statement rather than repeating that prefix across lines: factor out the shared part and group the divergent tails in braces.

```rust
// preferred
use crate::{bundle::Bundle, eid::Eid};

// avoid — repeats the `crate` prefix
use crate::bundle::Bundle;
use crate::eid::Eid;
```

This is the rustfmt `imports_granularity = "Crate"` layout; like the block order above, writing it by hand keeps `cargo fmt` a no-op if the option is ever enabled (it defaults to `Preserve`, so the merging is manual for now). Avoid the opposite, Java-style extreme of giving every item its own fully-qualified line.

**Avoid glob imports** (`use some_crate::*;`). Pulling every item from a crate into scope hides where each name comes from: when a dependency is later updated and removes or renames an item, the compiler reports an undefined symbol with no hint which glob it came from, and a newly-added upstream item can silently clash with a name from another glob. Explicit imports keep that traceable for the next maintainer.

`use super::*;` is the one sanctioned glob, and even then it is the exception, not the default. Reach for it only in a **leaf module** — where one logical module has been split across several deeply-interrelated files that legitimately share the parent's imports — or in a **unit-test module**, where `#[cfg(test)] mod tests { use super::*; … }` is the well-defined idiom (see [Tests](#tests)). In ordinary submodules, list imports explicitly rather than glob-importing the parent.

**Don't glob enum variants into scope** (`use SomeEnum::*;`) — it hides the variants' type and can obscure that an enum is being handled at all. Spell out `SomeEnum::Variant` in full, including in `match` arms. The visible type is the point, not noise; no local rename is worth losing it.

**Keep `use` at module scope.** Imports belong at the top of the file (or `mod` block), never inside a function body. A reader should find every path a function depends on in one place, and a function-local `use` — an alias especially — hides where a name really comes from.

**Name child modules through `self::`** when importing or re-exporting from them (`use self::foo::Foo;`, `pub use self::foo::Foo;`). The explicit `self::` distinguishes the child module from a same-named dependency, so introducing a crate called `foo` later cannot silently change what the path resolves to.

```rust
mod config;
mod error;

pub use self::config::Config;
pub use self::error::Error;
```

## Visibility

Control an item's visibility **at its own definition**, using `pub` or `pub(crate)` on the item. Do not widen or restrict visibility indirectly through re-exports or wrapper modules elsewhere — the `pub` keyword on the declaration is the single source of truth for how visible something is.

Inside a module that is itself private or `pub(crate)`, write plain `pub`, not `pub(crate)`. The enclosing module already caps visibility, so `pub(crate)` is redundant noise (the codebase does not rely on `unreachable_pub`).

Re-export public API at the crate root where it aids discoverability, but document each item at its definition, not at the re-export.

## Error Handling

Crates and subsystems define their own error enums with [`thiserror`](https://docs.rs/thiserror):

```rust
#[derive(Error, Debug)]
pub enum Error {
    /// Indicates that the bundle protocol version is unsupported.
    #[error("Unsupported bundle protocol version {0}")]
    InvalidVersion(u64),
}
```

- Give every variant a `#[error("…")]` message and a `///` doc line.
- Modules expose a local result alias: `pub type Result<T> = core::result::Result<T, Error>;`.
- Prefer **focused, leaf error types** for self-contained sub-parsers over reusing a crate-root `Error`. A small parser returning the whole crate's error enum leaks unrelated variants into its signature.
- Use `?` for propagation. Convert between error types with `#[from]` or explicit `map_err`, not by stringifying.
- Avoid `.unwrap()` / `.expect()` on fallible paths in library code. They are acceptable only where an invariant guarantees success — in which case use `.expect("reason the invariant holds")` so the message documents the invariant. Tests may unwrap freely.
- Never `panic!` in response to malformed wire input — parsers return errors. This is a security property, verified by fuzzing.

## 32-bit Safety

Hardy targets 32-bit platforms, so `usize` may be 32 bits wide. Wire formats carry 64-bit lengths and offsets.

**Never cast a wire-derived `u64` to `usize` with `as`.** Compare and validate in `u64` first, then convert with `try_from` and handle the error:

```rust
if len > MAX_LEN as u64 {
    return Err(Error::TooLong(len));
}
let len = usize::try_from(len).map_err(|_| Error::TooLong(len))?;
```

A silent `as usize` truncation on a 32-bit target is a parsing bug and a potential vulnerability.

## `no_std` and Feature Flags

The core libraries (`hardy-cbor`, `hardy-bpv7`, `hardy-bpa`) are `no_std`-compatible with an allocator. Keep them that way:

```rust
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;
```

- Import from `core` and `alloc`, not `std`, in `no_std` crates (`core::result::Result`, `alloc::vec::Vec`, etc.).
- Gate anything requiring the standard library behind a `std` feature, and propagate it to dependencies.
- Features should be additive — enabling one must not break another. Code must compile with `--all-features` (CI builds this way) and with default features.
- Document every feature flag in the crate-level doc comment (see the [rustdoc guide](rustdoc_style_guide.md)).

## Comments

- Doc comments (`///`, `/*! */`) document the public API — see the [rustdoc guide](rustdoc_style_guide.md). Public items require them; private and `pub(crate)` items do not.
- Use `//` line comments for non-obvious private logic and for surprising trait-impl behaviour.
- **Comments describe the present state of the code, not its history.** No "moved from X", "replaces the old Y", "now takes Z" porting narration — git holds that history, and stale migration notes mislead.
- Explain *why*, not *what*, when the *what* is already clear from the code.

## Async and Concurrency

- Use the runtime-agnostic primitives in `hardy-async` rather than reaching for `tokio` directly: `TaskPool` / `BoundedTaskPool` for spawning, the `spawn!` macro for tracing-instrumented tasks, cancellation tokens for shutdown.
- `hardy_async::sync::spin::{Mutex, RwLock, Once}` are for **O(1) critical sections only** — never hold a spin lock across an `.await`. For anything that awaits or does real work, use an async-aware lock.
- Subsystems (CLAs, services, routing agents) follow the **Trait + Sink** pattern: a trait with `on_register(sink, …)` / `on_unregister()`, a `Sink` back-channel, and dropping the `Sink` triggers automatic unregistration. New pluggable components should match this shape.

## Logging and Observability

- Use the `tracing` macros (`tracing::debug!`, `error!`, `instrument`) for logging and spans — not `println!` / `eprintln!` in library or server code.
- Keep log levels meaningful: `error!` for faults needing attention, `debug!`/`trace!` for diagnostics. Don't log per-bundle at `info!` on hot paths.

## Cargo and Dependencies

- Inherit shared package metadata from the workspace (`edition`, `rust-version`, `license`, `repository`) with `.workspace = true`.
- There is no `[workspace.dependencies]` table — dependencies are declared per crate. Keep versions consistent with what the rest of the workspace already uses (check the lockfile / a sibling crate before bumping).
- Keep dependencies minimal and justified; new third-party crates pass through `cargo deny` / the security-audit CI workflow.

## Tests

Where a test lives depends on what it needs to reach:

- **Integration tests — the default.** If a test exercises a crate's public API, put it in the crate's `tests/` directory (a sibling of `src/`). These compile as separate crates and can only see the public surface, which keeps them honest about what the crate actually exposes.
- **In-file unit tests — only for private access.** Add a `#[cfg(test)] mod tests { use super::*; … }` block at the bottom of a source file *only* when the test needs access to private module or file internals that are not (and should not be) public. This is the one place `use super::*;` is expected — the test module deliberately pulls in its parent's private items. When such a test module grows large relative to the source it covers, move it into a dedicated `tests.rs` file in the module (declared with `#[cfg(test)] mod tests;`) rather than letting it dominate the source file; as a child module it retains the same private access.

Conventions for both:

- Test functions are `snake_case` and name the scenario under test.
- Do not add rustdoc to test functions or test helpers.
- Parsers and protocol stream handlers also have fuzz targets under the crate's `fuzz/` directory; new wire-facing parsing code should come with one.

See the [test strategy](../test_strategy.md) for the overall testing approach across unit, integration, fuzz, and interop levels.

## Markdown and Prose (for docs you write)

- **One line per paragraph — do not hard-wrap at 80 columns.** Let the renderer wrap.
- Follow the relevant guide for the document type: [design docs](design_doc_style_guide.md), [READMEs](readme_style_guide.md), [coverage reports](coverage_report_style_guide.md).
