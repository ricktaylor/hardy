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
- **`use` statements form up to four blank-line-separated blocks**, in order: `super::`, then `std`/`core`/`alloc`, then `crate::`, then external crates. Merge imports from one path with `{}` rather than repeating the prefix.
- **Set visibility at the definition.** Use `pub`/`pub(crate)` on the item itself; do not widen or narrow it via re-exports elsewhere. Inside a private or `pub(crate)` module, write plain `pub`, not redundant `pub(crate)`.
- **32-bit safe.** Hardy targets 32-bit. Never `as usize` a wire-derived `u64` length — compare in `u64` first, then `try_from`.
- **Errors are `thiserror` enums** with a `#[error("…")]` per variant; modules expose `pub type Result<T> = core::result::Result<T, Error>`. Give sub-parsers focused leaf error types rather than reusing a crate-root `Error`.
- **`no_std` core.** `cbor`, `bpv7`, and `bpa` are `no_std` + `alloc`; gate `std` behind a feature, don't assume it.
- **Comments describe the present.** No "moved from / replaces the old X / now takes Y" porting narration — git holds that history.

## Documentation & prose

- Rustdoc on public items follows [`docs/style_guides/rustdoc_style_guide.md`](./docs/style_guides/rustdoc_style_guide.md).
- **Markdown: one line per paragraph — do not hard-wrap at 80 columns.**

## Style guides

All in [`docs/style_guides/`](./docs/style_guides/):

| Topic | Guide |
|-------|-------|
| Rust code conventions | [code_style_guide.md](./docs/style_guides/code_style_guide.md) |
| Rustdoc comments | [rustdoc_style_guide.md](./docs/style_guides/rustdoc_style_guide.md) |
| Per-crate design docs | [design_doc_style_guide.md](./docs/style_guides/design_doc_style_guide.md) |
| Per-crate READMEs | [readme_style_guide.md](./docs/style_guides/readme_style_guide.md) |
| Test coverage reports | [coverage_report_style_guide.md](./docs/style_guides/coverage_report_style_guide.md) |

For the overall testing approach, see [`docs/test_strategy.md`](./docs/test_strategy.md) — a deliverable document, not a style guide.
