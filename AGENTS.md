# Hardy — agent guide

Hardy is a performant, RFC 9171-compliant, extensible BPv7 Delay-Tolerant Networking (DTN) implementation, written in async Rust. It is a Cargo workspace on the **stable** toolchain; the edition and MSRV are defined in the workspace [`Cargo.toml`](./Cargo.toml) (`[workspace.package]`). See [`README.md`](./README.md) for the full feature list and the complete crate inventory.

## Workspace layout

The workspace is split into many small crates. The ones you will touch most often:

- `cbor/` — RFC 8949 canonical CBOR codec (`no_std`).
- `bpv7/` — RFC 9171 bundle format: parsing, building, editing, BPSec (`no_std`).
- `bpa/` — the Bundle Processing Agent library: routing, dispatch, filter pipeline, RIB, storage/CLA/service registries.
- `bpa-server/` — the binary that wires the BPA together; closed-source extensions register against the **unmodified** `bpa` crate via public traits.
- `eid-patterns/`, `async/`, `proto/`, `otel/` — supporting libraries.
- `*-storage/` — pluggable storage backends (localdisk, sqlite, postgres, s3).
- `tcpclv4/`, `file-cla/`, `bibe/` — convergence layers; `tvr/` — Time-Variant Routing agent.
- `tools/`, `bpv7/tools/`, `cbor/tools/` — CLI tools (`bp`, `bundle`, `cbor`).

`tests/interop/mtcp` is excluded from the workspace and built separately.

## Build, test, lint

Proto-dependent crates require `protoc` (CI installs `protobuf-compiler`). Common commands:

```bash
cargo build --release                          # build everything
cargo test --workspace --all-features          # run the test suite
cargo fmt                                       # format
cargo clippy --all-targets --all-features      # lint
```

**A change is not done until these CI gates pass** — run them before handing work back:

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features --workspace
```

Clippy is enforced with `-D warnings`: no warnings are allowed to land.

## Code style — the essentials

Full reference: [`docs/style_guides/code_style_guide.md`](./docs/style_guides/code_style_guide.md). Existing code does not all comply — apply these only to files you are already changing for another reason, never as a standalone reformatting sweep. The rules most easily missed:

- **Write idiomatic Rust.** Default to community-standard idioms (the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) and [Rust Style Guide](https://doc.rust-lang.org/style-guide/)); the guide only records project-specific conventions, and Clippy enforces most of the rest.
- **Formatting is `rustfmt`-decided.** Run `cargo fmt`; never hand-format to deviate from it.
- **`use` statements form up to three blank-line-separated blocks**, in order: `std`/`core`/`alloc`, then third-party crates, then local (`self::`/`super::`/`crate::`, the order rustfmt sorts them within the block) — the rustfmt `group_imports = "StdExternalCrate"` order. Collapse same-crate imports into one nested `use` (`imports_granularity = "Crate"`, e.g. `use crate::{bundle::Bundle, eid::Eid};`); avoid glob imports (`use foo::*;`) except `super::*` in leaf/test modules.
- **Import items; use the bare name.** Every type, trait, function, variant, and constant a file references is imported at the top and used unqualified; a multi-segment path at the use site (`std::sync::Arc::new`, `core::num::NonZeroU32`) is a defect. The one exception is a name collision between two imports (alias or qualify the less-central one, with a comment). Trait-method calls, macro paths, and `Type::assoc` on an already-imported type are exempt. Full rule and examples: [Imports and `use` Blocks](./docs/style_guides/code_style_guide.md#imports-and-use-blocks).
- **Set visibility at the definition.** Use `pub`/`pub(crate)` on the item itself; do not widen or narrow it via re-exports elsewhere. Inside a private or `pub(crate)` module, write plain `pub`, not redundant `pub(crate)`.
- **32-bit safe.** Hardy targets 32-bit. Never `as usize` a wire-derived `u64` length — compare in `u64` first, then `try_from`.
- **Errors are `thiserror` enums** with a `#[error("…")]` per variant; modules expose `pub type Result<T> = core::result::Result<T, Error>`. Give sub-parsers focused leaf error types rather than reusing a crate-root `Error`.
- **`no_std` core.** `cbor`, `bpv7`, and `bpa` are `no_std` + `alloc`; gate `std` behind a feature, don't assume it.
- **Secrets never reach `Display`, `Debug`, or logs.** Key material, tokens, and credentials get a redacting `Debug` (length or kid, never bytes), are never interpolated into error messages, and raw key bytes live in `zeroize::Zeroizing` (`bpsec::key::Type` is the pattern).
- **Comments describe the present.** No "moved from / replaces the old X / now takes Y" porting narration — git holds that history.

## Testing

Full conventions: [`docs/style_guides/test_style_guide.md`](./docs/style_guides/test_style_guide.md). The rules most easily missed:

- **Deterministic, never timed.** A test must never use `sleep` or a timing margin to order two operations: synchronize on the event itself (a signal the code raises, a capacity-1 channel rendezvous, a `Barrier`, bind-before-spawn), or add a minimal `#[cfg(test)]` hook when a private transition is otherwise unobservable.
- **`timeout()` bounds regressions only.** Use it solely as a generous hang failsafe on an event-driven wait, with the comment `the timeout only bounds a regression`; never size it to "should finish in time", and never prove absence with a quiet window (drive a real barrier like a completed `shutdown().await`, then assert empty).
- **Paused clock for time-dependent behaviour.** `#[tokio::test(start_paused = true)]` plus `tokio::time::advance`; never wait for a real timeout.
- **No shared ambient state.** Ephemeral ports (`:0`), per-process temp dirs (`std::process::id()`), and RAII-guarded env vars in `#[serial]` tests.
- **A test must be able to fail for the behaviour it names.** Exercise the real production path (never a re-implementation of the algorithm), and assert the specific value or typed error variant, never a bare `is_err()` or a `to_string().contains(...)`.
- **Placement:** public-API tests in the crate's `tests/`; private-internal tests in an inline `#[cfg(test)] mod tests`. No test or fixture file under `src/` (no `src/tests.rs`, no `src/test_util.rs`); shared fixtures live in an inline `#[cfg(test)] pub mod tests` cross-imported by path, or in `tests/common/mod.rs` when shared between integration-test binaries.
- **No hard-coded cryptographic values, even in tests.** Never write literal keys/IVs/salts, including in `#[cfg(test)]` code: generate immaterial values via the crate's own rand helper; only externally pinned vectors (spec appendices, conformance/PICS suites, interop captures) stay verbatim.

## Documentation & prose

- Rustdoc on public items follows [`docs/style_guides/rustdoc_style_guide.md`](./docs/style_guides/rustdoc_style_guide.md).
- **Markdown: one line per paragraph — do not hard-wrap at 80 columns.**

## Style guides

All in [`docs/style_guides/`](./docs/style_guides/):

| Topic | Guide |
|-------|-------|
| Rust code conventions | [code_style_guide.md](./docs/style_guides/code_style_guide.md) |
| Test conventions | [test_style_guide.md](./docs/style_guides/test_style_guide.md) |
| Rustdoc comments | [rustdoc_style_guide.md](./docs/style_guides/rustdoc_style_guide.md) |
| Per-crate design docs | [design_doc_style_guide.md](./docs/style_guides/design_doc_style_guide.md) |
| Per-crate READMEs | [readme_style_guide.md](./docs/style_guides/readme_style_guide.md) |
| Test coverage reports | [coverage_report_style_guide.md](./docs/style_guides/coverage_report_style_guide.md) |

For the overall testing approach, see [`docs/test_strategy.md`](./docs/test_strategy.md) — a deliverable document, not a style guide.
