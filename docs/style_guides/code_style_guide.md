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

The rules first, as a table an import decision can be checked against, then the detail behind each. Every rule has an ID so a review can cite it. `cargo fmt` enforces only the sorting inside a block; everything else here is written by hand (see [What rustfmt does and does not do](#what-rustfmt-does-and-does-not-do)).

### Rules at a glance

| ID | Rule | Write | Not |
|----|------|-------|-----|
| I1 | Import every item a file references and use the bare name. A `::` path at a use site is a defect. Functions included: this departs from the Rust Book's `module::function()` convention, for one rule across all items. | `Arc<Registry>` | `std::sync::Arc<hardy_bpa::cla::Registry>` |
| I2 | Exempt from I1, because there is no import to hoist: macro paths, `Type::assoc` on an imported type, trait-method calls. | `tracing::debug!(..)`, `NonZeroU32::new(n)`, `value.method()` | — |
| I3 | Two imports would share a name: alias the less-central one `<Origin><Name>`, with a one-line comment at the import. | `use hardy_cbor::decode::Error as CborError;` | `hardy_cbor::decode::Error` spelled in a signature |
| I4 | The colliding pair is two role-named items, or two modules' `Error`/`Result`: import both modules and qualify both. No comment. | `Box<dyn cla::Sink>` beside `Arc<dyn routing::Sink>`; `cla::Result<()>` beside `service::Result<Eid>` | `Sink as ClaSink` |
| I5 | A trait imported only for its methods is anonymous. | `use sqlx::Connection as _;` | `use sqlx::Connection;` when the name is never written |
| I6 | A crate that links `std` imports from `std`. A `no_std` crate imports from `core` and `alloc`, and takes `Arc`, `Weak`, the maps and the sets from its crate root. | `use std::sync::Arc;` in `bpa-server`; `use crate::{Arc, HashMap};` in `bpa` | `use core::fmt;` in `bpa-server`; `use hashbrown::HashMap;` in a `bpa` file |
| I7 | Up to three blank-line-separated blocks, in order: `std`/`core`/`alloc`, external crates, `self`/`super`/`crate`. One nested `use` per crate. Child modules through `self::`. | the example under [Laying out the `use` block](#laying-out-the-use-block) | `use crate::a::A;` on one line and `use crate::b::B;` on the next |
| I8 | `use` lives at the top of the file or `mod` block, never inside a function. | — | `fn f() { use std::fmt::Write; … }` |
| I9 | No globs except the four under [Globs](#globs). Never glob an enum's variants. | `use super::*;` in `mod tests` | `use hardy_bpa::cla::*;`, `use Kind::*;` |
| I10 | Re-exports: a module root re-exports its private submodules' items; the crate root re-exports dependency types that appear in its public API; nothing else re-exports. | `pub use self::address::Address;` in `cla/mod.rs` | `pub use crate::cla::Address;` from `dispatcher` |

### How to write a reference

For every type, trait, function, variant or constant a file names, decide in this order:

1. Defined in this file or module: the bare name.
2. Defined elsewhere: add it to the `use` block (I7) and use the bare name (I1). A macro path, a `Type::assoc` on an imported type, or a method call needs nothing (I2).
3. The bare name would collide with another import. If the pair is two role-named items (`cla::Sink` and `routing::Sink`; likewise `routing::Agent`, `cla::Address`) or two modules' `Error`/`Result`, import the modules and qualify both (I4). Otherwise alias the less-central one `<Origin><Name>` and comment the import (I3). Reuse an alias the workspace already uses rather than coining one: `hardy_bpv7::eid::Service` is `ServiceId`; `Error` is `CborError`, `Bpv7Error`, `ClaError` or `EditorError` after its crate or module; `Result` from storage is `StorageResult`; `Bundle` from `hardy_bpv7` is `Bpv7Bundle`; `Flags` are `BlockFlags` or `BundleFlags`.
4. A trait whose name never appears in the file, in a `dyn`, an `impl … for`, a bound or a `Trait::method` call: `use path::Trait as _;` (I5).

The four together:

```rust
use std::sync::Arc;

use hardy_bpa::{
    cla, routing,
    service::{self, Application, Service},
};
// `Service` is the whole-bundle service trait above; the EID service number is aliased (I3).
use hardy_bpv7::eid::{Eid, Service as ServiceId};
use sqlx::Connection as _;

struct Node {
    // Two sinks in one file: both through their module (I4).
    cla_sink: Box<dyn cla::Sink>,
    agent_sink: Arc<dyn routing::Sink>,
    // One of each: bare (I1).
    app: Arc<dyn Application>,
    svc: Arc<dyn Service>,
}

// Two modules' results in one file: both through their module (I4).
fn attach(node: &Node, id: ServiceId) -> cla::Result<Eid> { /* … */ }
fn detach(node: &Node) -> service::Result<()> { /* … */ }
```

### Laying out the `use` block

```rust
use core::num::NonZeroUsize;
use std::sync::Arc;

use thiserror::Error;
use tracing::{debug, warn};

use self::registry::Registration;
use super::Dispatcher;
use crate::{bundle::Bundle, eid::Eid};
```

- Three blocks at most, in that order; a block with nothing in it is omitted, never left as a blank line (I7).
- One nested `use` per crate or per local root: factor the shared prefix and brace the tails. Not one fully qualified line per item.
- A child module is named through `self::` (`use self::foo::Foo;`, `pub use self::foo::Foo;`), so a dependency later called `foo` cannot capture the path.
- `use` sits at module scope, at the top of the file or of the `mod` block it belongs to (I8). A function-local `use`, an alias above all, hides where a name comes from.

### Where names come from

- A crate that links `std` imports from `std` (`std::sync::Arc`, `std::time::Duration`, `std::fmt`), not from the `core` or `alloc` paths that re-export the same items (I6). This is the opposite of Clippy's opt-in `std_instead_of_core`, which this workspace does not enable: only the six crates below build `no_std`, and they are named.
- A `no_std` crate (`cbor`, `bpv7`, `bpa`, `eid-patterns`, `btpu`, `bibe`) imports from `core` and `alloc`, and gates anything `std`-only behind its `std` feature.
- A per-feature choice of implementation is made once, at the crate root, and files import the chosen name from `crate::`. In `bpa` that is `Arc` and `Weak` from `alloc::sync`, and `HashMap`, `HashSet`, `hash_map`, `BTreeMap`, `BTreeSet`, `btree_map` from `std::collections` or `hashbrown`. A file writes `use crate::{Arc, HashMap};` and never names `alloc::sync`, `std::collections` or `hashbrown` itself.

### Globs

| Glob | Where | Why it is allowed |
|------|-------|-------------------|
| `use super::*;` | a `#[cfg(test)] mod tests`, or a leaf module split across files that share the parent's imports | the standard idiom; every name comes from one known parent |
| `use <dep>::prelude::*;` | `chumsky`, `base64`, `rand` | a prelude exists to be globbed, and the crate documents its contents |
| `use trace_err::*;` | anywhere | extension traits only; no name from it is ever written |
| `use proto::<file>::*;` | inside `hardy-proto`, over the generated modules | generated code whose names the `.proto` file fixes |

Everything else imports by name (I9), so an item removed or renamed upstream fails at the import that names it, not at an unexplained undefined symbol. An enum's variants are never globbed: write `Kind::Variant`, in `match` arms too, so the type stays visible.

### Re-exports

A module root re-exports the public items of its private submodules (`mod address;` then `pub use self::address::{Address, AddressType};`), so how the module is split into files is not part of its path. The crate root re-exports a dependency's type only when that type appears in the crate's public API (`pub use bytes::Bytes;`, `pub use hardy_async::async_trait;`), so a caller can name it without adding the dependency. Nothing else re-exports (I10): a consumer imports an item from the module that declares it, never through an incidental re-export, and each item is documented at its definition, not at the re-export. Visibility itself is set at the definition, see [Visibility](#visibility).

### What rustfmt does and does not do

- Does: sort the items within a block and within braces, and keep the blank lines between blocks.
- Does not: order the blocks, merge per-crate `use`s, or move a `use` out of a function. The matching options, `group_imports = "StdExternalCrate"` and `imports_granularity = "Crate"`, are nightly-only; writing their layout by hand keeps `cargo fmt` a no-op if they stabilise.

### Before handing back

- No `::` path in a body except the I2 cases and I4 pairs.
- Every alias is `<Origin><Name>`, reuses the workspace's existing alias for that item, and is commented at the import.
- Blocks in I7 order, no empty block, no `use` inside a function.
- Every glob appears in the table under [Globs](#globs).

## Visibility

Control an item's visibility **at its own definition**, using `pub` or `pub(crate)` on the item. Do not widen or restrict visibility indirectly through re-exports or wrapper modules elsewhere — the `pub` keyword on the declaration is the single source of truth for how visible something is.

Inside a module that is itself private or `pub(crate)`, write plain `pub`, not `pub(crate)`. The enclosing module already caps visibility, so `pub(crate)` is redundant noise (the codebase does not rely on `unreachable_pub`).

Where re-exports live is import rule I10, under [Re-exports](#re-exports).

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

## Cryptographic Hygiene

- **MAC, tag, and signature verification MUST be constant-time.** Never compare a computed authenticator against the received one with `==` — a short-circuiting comparison leaks how many leading bytes matched. Use the primitive's own verify API (`Mac::verify_slice`, an AEAD's built-in tag check); if none exists, compare through `subtle::ConstantTimeEq`.
- **Cryptographic randomness comes from the OS-backed RNG only** (`rand::rngs::SysRng`). A non-cryptographic PRNG (`SmallRng`) is permitted only for values with no security role (public identifiers, cache sampling) and the use site MUST carry a comment saying so.
- **Plaintext key material MUST NOT transit unzeroized buffers.** Allocate the buffer inside `zeroize::Zeroizing` before any plaintext is written into it, so every exit path (including `?` and unwind) wipes it in `Drop` and no conversion can strand an unwiped copy on the heap; `rfc9173::key_wrap::unwrap` is the pattern.

## Logging and Observability

- Use the `tracing` macros (`tracing::debug!`, `error!`, `instrument`) for logging and spans — not `println!` / `eprintln!` in library or server code.
- Keep log levels meaningful: `error!` for faults needing attention, `debug!`/`trace!` for diagnostics. Don't log per-bundle at `info!` on hot paths.
- **Secrets never reach `Display`, `Debug`, or logs — no exceptions.** Raw symmetric key bytes live only in `bpsec::key::SecretBytes` (self-redacting `Debug`, no `Display`, zeroized on drop), so containers embedding it can derive `Debug`. Secret types owned by other crates (session tokens, credentials) MUST have a redacting `Debug` (print the length or the key id, never the bytes), MUST NOT be interpolated into error messages or log lines, and MUST wrap raw bytes in `zeroize::Zeroizing` so they are wiped on drop.

## Cargo and Dependencies

- Inherit shared package metadata from the workspace (`edition`, `rust-version`, `license`, `repository`) with `.workspace = true`.
- There is no `[workspace.dependencies]` table — dependencies are declared per crate. Keep versions consistent with what the rest of the workspace already uses (check the lockfile / a sibling crate before bumping).
- Keep dependencies minimal and justified; new third-party crates pass through `cargo deny` / the security-audit CI workflow.

## Tests

Where a test lives depends on what it needs to reach:

- **Integration tests — the default.** If a test exercises a crate's public API, put it in the crate's `tests/` directory (a sibling of `src/`). These compile as separate crates and can only see the public surface, which keeps them honest about what the crate actually exposes.
- **In-file unit tests — only for private access.** Add a `#[cfg(test)] mod tests { use super::*; … }` block at the bottom of a source file *only* when the test needs access to private module or file internals that are not (and should not be) public. This is the one place `use super::*;` is expected — the test module deliberately pulls in its parent's private items. A test module that has grown large is not a reason to hive it into a dedicated `src/tests.rs`: there are **no dedicated test or fixture files under `src/`**. If the large module's tests only touch the public API, move those to `tests/`; keep the genuinely internal ones inline, and put shared fixtures in an inline `#[cfg(test)] pub mod tests` cross-imported by path (`use crate::session::tests::MockSink;`).

Beyond placement, tests must be **deterministic** (never synchronize with a sleep or a timing margin; `timeout` bounds regressions only, never orders operations or proves absence) and must be **able to fail for the behaviour they name** (exercise the real production path, assert the specific value or typed error variant, never a bare `is_err()`). The full conventions (the synchronization toolbox, the paused clock, ambient-state isolation, and the fixture layout) are in the [test style guide](test_style_guide.md).

Conventions for all tests:

- Test functions are `snake_case` and name the scenario under test.
- Do not add rustdoc to test functions or test helpers.
- Parsers and protocol stream handlers also have fuzz targets under the crate's `fuzz/` directory; new wire-facing parsing code should come with one.

See the [test style guide](test_style_guide.md) for the full testing conventions and the [test strategy](../test_strategy.md) for the overall approach across unit, integration, fuzz, and interop levels.

## Markdown and Prose (for docs you write)

- **One line per paragraph — do not hard-wrap at 80 columns.** Let the renderer wrap.
- Follow the relevant guide for the document type: [design docs](design_doc_style_guide.md), [READMEs](readme_style_guide.md), [coverage reports](coverage_report_style_guide.md).
