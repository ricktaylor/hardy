# hardy-plugin-abi Design

Runtime plugin loading for Hardy BPA — shared libraries provide CLA, storage, filter, and policy implementations without recompiling the server.

## Design Goals

Hardy's subsystems are trait-based and extensible by design, but the only way to provide a new implementation is to add a Cargo dependency and recompile. This is a problem for three categories of use:

- **Proprietary or closed-source implementations** that cannot be merged upstream or distributed as source.
- **Non-standards-track protocols** (e.g., MTCP/STCP for interop testing with ION and ud3tn) that should not appear in the main binary.
- **Third-party components** that need independent release cadences without forking the server.

The plugin system allows operators to load shared libraries by explicit path in the configuration file or on the command line. The design prioritises simplicity and safety over generality — there is no stable ABI, no hot-reload, no directory scanning, and no C plugin interface.

## Architecture Overview

The system has two sides, both provided by `hardy-plugin-abi`:

- **Plugin-side** (default features): FFI boundary mechanics — result types, panic guards, config parsing, and an ABI version token. Plugin crates use these to implement entry points safely. Trait definitions come from `hardy-bpa` directly; the ABI crate does not re-export them.

- **Host-side** (`host` feature): Loading infrastructure — open a shared library, verify its ABI token matches the host, call the appropriate factory function, and return the trait object. This is a library, not server-specific code, so both `hardy-bpa-server` and `bp ping` can load plugins without duplicating logic.

The separation into a single crate with feature-gated facets keeps the plugin crate's dependency tree minimal (no `libloading`) while giving any host binary access to the loader.

### Loading Model: Explicit Paths Only

Plugins are loaded by **explicit file path** specified in the configuration file or on the command line. The system does not scan directories for `.so` files or resolve plugin names to paths. Every plugin that gets loaded is one that an operator deliberately configured.

This is a deliberate security decision. Directory scanning would mean that any `.so` file dropped into a directory — whether by an attacker, a misconfigured package manager, or a stale build artifact — would be loaded and executed with the full privileges of the BPA process. Since plugins receive a `Sink` with the ability to dispatch bundles, modify routing, and access the BPA's internal state, loading untrusted code would be a complete compromise.

In the server config, the CLA `type` field is either a built-in name (`tcpclv4`, `file-cla`) or an absolute path to a shared library. In tools like `bp ping`, the `--cla` flag takes a path directly.

## Key Design Decisions

### Same-compiler ABI constraint (no `abi_stable`)

Passing `Arc<dyn Trait>` across a shared-library boundary requires identical type layouts on both sides. Rather than adopting `abi_stable` (which would require rewriting all trait signatures to use its wrapper types), the design accepts the constraint that plugins must be compiled with the same `rustc` version and `hardy-bpa` crate version as the host. This is enforced at runtime by an ABI token — a string embedding the crate version and Rust compiler version, checked before any other symbol is called.

The `abi_stable` approach was rejected because the surface area is too large: `Bytes`, `Arc`, `HashMap`, `Sender<T>`, and many other Rust types appear in the trait signatures for storage, filters, and policies. Wrapping all of them would be a pervasive and invasive change to `hardy-bpa` for a benefit (cross-compiler plugins) that is not needed in practice.

### Host-side loading in the ABI crate, not the server

The original design placed all loading code in `hardy-bpa-server`. This was changed when it became clear that `bp ping` also needs to load CLA plugins for interop testing. Putting the loader in `hardy-plugin-abi` (behind a `host` feature) avoids duplicating the loading logic across binaries and ensures consistent ABI checks.

### Traits from `hardy-bpa`, not re-exported

Plugin crates depend on `hardy-bpa` directly for trait definitions (`Cla`, `Sink`, `ReadFilter`, `BundleStorage`, etc.). The ABI crate's role is strictly FFI mechanics — shared result types, panic guards, config parsing, and loading. Re-exporting traits would create an unnecessary indirection layer and a false impression that the ABI crate mediates the trait contract.

### `extern "C"` entry points with Rust types

Entry points use `extern "C"` linkage for symbol lookup (`dlsym`), but the function signatures use Rust types (`Arc<dyn Cla>`, `PluginResult<T>`). This is not FFI-safe by the language's definition, but is safe under the same-`rustc`-version constraint. The alternative — a pure C ABI with opaque pointers and manual vtables — would add significant complexity for no practical benefit, since plugins are expected to be Rust crates compiled alongside the host.

### Panic guards at every entry point

A panic unwinding across an `extern "C"` boundary is undefined behaviour. The `guard()` and `guard_factory()` helpers wrap entry point bodies in `catch_unwind`, converting panics to error codes. This is a defence-in-depth measure — well-written plugins should not panic, but the host must not crash if they do.

## Integration

### With `hardy-bpa`

The ABI crate depends on `hardy-bpa` only when the `host` feature is enabled (for `Arc<dyn Cla>` in the factory function type). Plugin crates depend on `hardy-bpa` directly. There are no changes required to `hardy-bpa` for CLA plugin loading.

Future plugin types (storage, filters, policy factories) will require upstream changes to `hardy-bpa`, notably an `EgressPolicyFactory` trait and a factory registry in `Bpa` so that policy plugins can register named factories and CLA config can reference them by name.

### With `hardy-bpa-server`

The server adds a `dynamic-plugins` feature gating `hardy-plugin-abi/host`. The `ClaConfig` enum gains an `Other` catch-all variant (via `#[serde(untagged)]`) that captures unrecognised `type` values and their remaining config fields as JSON. During `clas::init()`, the `type` value is treated as a file path and loaded via `load_cla_plugin()`.

### With `hardy-tools` (`bp ping`)

`bp ping` gains `--cla <name-or-path>` and `--cla-config <json>` flags. When `--cla` is a file path, the tool loads the plugin via `load_cla_plugin()`, registers the CLA with its inline BPA, and the CLA's `on_register()` establishes peer routing via `sink.add_peer()`. The `--cla-config` JSON is passed verbatim to the plugin — `bp ping` never parses CLA-specific config fields, keeping the tool fully generic. Built-in CLA names (`tcpclv4`, future `quicl`) use direct construction as today.

### With plugin crates

A plugin crate is a `cdylib` that depends on `hardy-plugin-abi` (for FFI helpers) and `hardy-bpa` (for traits). It exports `HARDY_ABI_TOKEN` and one or more factory/registration functions. The MTCP/STCP CLA at `tests/interop/mtcp/` is a complete working example.

## Lifecycle and Safety

Plugin shared libraries must outlive the trait objects they provide. The host declares `Library` handles before the `Bpa` so that Rust's reverse drop order ensures `bpa` is dropped (releasing all trait objects) before libraries are unloaded.

All plugin traits require `Send + Sync`. Async methods execute on the host's Tokio runtime — plugins must not create their own. CLA plugins may spawn background tasks (e.g., a listener loop) using `hardy_async::spawn!` on the runtime's task pool.

## Testing

- **ABI crate unit tests**: load test `.so`, verify ABI check, error handling for missing symbols, wrong token, and bad config.
- **CLA plugin integration**: see `tests/interop/mtcp/` — the MTCP/STCP CLA plugin exercises the full factory → register → forward → shutdown lifecycle.
- **Interop tests**: `tests/interop/ION/` and `tests/interop/ud3tn/` use the MTCP plugin against real DTN implementations in Docker.
- **ABI mismatch test**: build a plugin with a different `hardy-plugin-abi` version, verify the host rejects it with a clear error message.
