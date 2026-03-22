# hardy-plugin-abi Design

Runtime plugin loading for Hardy BPA — shared libraries provide CLA, storage, filter, and policy implementations without recompiling the server.

## Design Goals

Hardy's subsystems are trait-based and extensible by design, but the only way to provide a new implementation is to add a Cargo dependency and recompile. This is a problem for three categories of use:

- **Proprietary or closed-source implementations** that cannot be merged upstream or distributed as source.
- **Non-standards-track protocols** (e.g., MTCP/STCP for interop testing with ION and ud3tn) that should not appear in the main binary.
- **Third-party components** that need independent release cadences without forking the server.

The plugin system allows operators to load shared libraries by explicit path in the configuration file or on the command line. The design prioritises simplicity and safety over generality — there is no stable ABI, no hot-reload, no directory scanning, and no C plugin interface.

## Architecture Overview

The crate has two facets controlled by the `host` feature:

- **Plugin-side** (default features): FFI boundary mechanics — result types, panic guards, config parsing, ABI version token, and the `export_cla!` macro for boilerplate-free entry points. Trait definitions come from `hardy-bpa` directly; the ABI crate does not re-export them.

- **Host-side** (`host` feature): Plugin loading and the apartment proxy system. Organised as a `host` submodule with trait-specific submodules:

```
plugin-abi/src/
├── lib.rs                 — public API, feature gates
├── client.rs              — export_cla! macro (plugin-side)
└── host/
    ├── mod.rs             — load_and_check, Library, PluginLoadError, apartment docs
    └── cla.rs             — load_cla_plugin, ClaProxy, ProxySink
```

Future trait families (Service Sink, RoutingAgent Sink) will add sibling modules alongside `cla.rs`, each containing their own loader and apartment proxy.

### Loading Model: Explicit Paths Only

Plugins are loaded by **explicit file path** specified in the configuration file or on the command line. The system does not scan directories for `.so` files or resolve plugin names to paths. Every plugin that gets loaded is one that an operator deliberately configured.

This is a deliberate security decision. Directory scanning would mean that any `.so` file dropped into a directory — whether by an attacker, a misconfigured package manager, or a stale build artifact — would be loaded and executed with the full privileges of the BPA process. Since plugins receive a `Sink` with the ability to dispatch bundles, modify routing, and access the BPA's internal state, loading untrusted code would be a complete compromise.

In the server config, the CLA `type` field is either a built-in name (`tcpclv4`, `file-cla`) or a path to a shared library. In tools like `bp ping`, the `--cla` flag takes a path directly.

## Key Design Decisions

### Same-compiler ABI constraint (no `abi_stable`)

Passing `Arc<dyn Trait>` across a shared-library boundary requires identical type layouts on both sides. Rather than adopting `abi_stable` (which would require rewriting all trait signatures to use its wrapper types), the design accepts the constraint that plugins must be compiled with the same `rustc` version and `hardy-bpa` crate version as the host. This is enforced at runtime by an ABI token — a string embedding the crate version and Rust compiler version, checked before any other symbol is called.

The `abi_stable` approach was rejected because the surface area is too large: `Bytes`, `Arc`, `HashMap`, `Sender<T>`, and many other Rust types appear in the trait signatures for storage, filters, and policies. Wrapping all of them would be a pervasive and invasive change to `hardy-bpa` for a benefit (cross-compiler plugins) that is not needed in practice.

### Apartment pattern for cross-runtime isolation

`cdylib` plugins statically link all their dependencies, including tokio. This means the plugin has its own tokio runtime with separate thread-local storage. Calls from the plugin's runtime threads into the host's BPA code (via trait method vtables) fail because the host's `tokio::spawn` can't find the host's runtime in the plugin's TLS.

This is solved with an apartment pattern inspired by Windows COM. Each plugin trait pair (e.g., `Cla`/`Sink`, future `Service`/`ServiceSink`) is wrapped in a proxy that separates the host's and plugin's runtime contexts:

- **Outbound calls** (host → plugin: `on_register`, `forward`, `on_unregister`) are dispatched via a channel to a dispatcher task running on the **plugin's** runtime. The plugin's code executes on threads with the plugin's TLS, so `tokio::spawn`, `TcpStream::connect`, etc. all work.

- **Inbound calls** (plugin → host: `dispatch`, `add_peer`, `remove_peer`) are dispatched via a channel to a dispatcher task running on the **host's** runtime. The host's BPA code executes on threads with the host's TLS, so `tokio::spawn` and internal BPA operations work.

- **Runtime creation** uses a two-symbol pattern: the `export_cla!` macro exports a `hardy_create_runtime` function that creates a tokio runtime using the **plugin's** copy of tokio (so worker threads have the plugin's TLS). The host calls this during loading and passes the runtime to the proxy. The plugin author never sees this — the macro handles it.

The wrapping is entirely internal to the loader function (e.g., `load_cla_plugin`). Callers receive a plain `Arc<dyn Cla>` — the proxy is invisible.

### Library lifetime managed by the proxy

Each plugin proxy holds both the plugin's trait object and the `Library` handle. When the BPA drops the trait object (during unregistration or shutdown), the proxy drops the plugin's trait object first (Rust struct drop order), then unloads the shared library. Callers receive a plain `Arc<dyn Trait>` — no tuple, no separate lifetime management.

### Host-side loading in the ABI crate, not the server

The original design placed all loading code in `hardy-bpa-server`. This was changed when it became clear that `bp ping` also needs to load CLA plugins for interop testing. Putting the loader in `hardy-plugin-abi` (behind a `host` feature) avoids duplicating the loading logic across binaries and ensures consistent ABI checks and apartment wrapping.

### Traits from `hardy-bpa`, not re-exported

Plugin crates depend on `hardy-bpa` directly for trait definitions (`Cla`, `Sink`, `ReadFilter`, `BundleStorage`, etc.). The ABI crate's role is strictly FFI mechanics. Re-exporting traits would create an unnecessary indirection layer and a false impression that the ABI crate mediates the trait contract.

### `extern "C"` entry points with Rust types

Entry points use `extern "C"` linkage for symbol lookup (`dlsym`), but the function signatures use Rust types (`Arc<dyn Cla>`, `PluginResult<T>`). This is not FFI-safe by the language's definition, but is safe under the same-`rustc`-version constraint. The alternative — a pure C ABI with opaque pointers and manual vtables — would add significant complexity for no practical benefit, since plugins are expected to be Rust crates compiled alongside the host.

### Panic guards at every entry point

A panic unwinding across an `extern "C"` boundary is undefined behaviour. The `guard()` and `guard_factory()` helpers wrap entry point bodies in `catch_unwind`, converting panics to error codes. The `export_cla!` macro applies this automatically.

## Integration

### With `hardy-bpa`

The ABI crate depends on `hardy-bpa` only when the `host` feature is enabled (for `Arc<dyn Cla>` in the factory function type and `Sink` in the apartment proxy). Plugin crates depend on `hardy-bpa` directly. There are no changes required to `hardy-bpa` for CLA plugin loading.

### With `hardy-bpa-server`

The server adds a `dynamic-plugins` feature gating `hardy-plugin-abi/host`. The `ClaConfig` enum gains an `Other` catch-all variant (via `#[serde(untagged)]`) that captures unrecognised `type` values and their remaining config fields as JSON. During `clas::init()`, the `type` value is treated as a file path and loaded via `load_cla_plugin()`. The apartment proxy and library lifetime are handled internally — the server just registers the returned `Arc<dyn Cla>` with the BPA.

### With `hardy-tools` (`bp ping`)

`bp ping` gains `--cla <name-or-path>` and `--cla-config <json>` flags. When `--cla` is a file path, the tool calls `load_cla_plugin()` and registers the result with its inline BPA. The CLA's `on_register()` establishes peer routing via the proxied `sink.add_peer()`. The `--cla-config` JSON is passed verbatim to the plugin — `bp ping` never parses CLA-specific config fields, keeping the tool fully generic.

### With plugin crates

A plugin crate is a `cdylib` that depends on `hardy-plugin-abi` (for FFI helpers) and `hardy-bpa` (for traits). It exports entry points via an `export_*!` macro (e.g., `export_cla!`). The macro handles runtime creation, ABI token export, and factory boilerplate — the plugin author just implements the trait. The MTCP/STCP CLA at `tests/interop/mtcp/` is a complete working example.

## Lifecycle and Safety

Each plugin proxy manages the full lifecycle: it holds the `Library` handle alongside the plugin's trait object, and wraps the corresponding Sink in an apartment proxy during registration. When the BPA unregisters the plugin, the proxy drops the trait object then unloads the library — no external lifetime management needed.

The apartment has two dispatcher tasks — one on each runtime. The inbound dispatcher (host runtime) receives Sink calls from the plugin and executes them in the host's context. The outbound dispatcher (plugin runtime) receives trait calls from the host and executes them in the plugin's context. Both exit when their channel closes (proxy dropped) or on explicit unregistration.

All plugin traits require `Send + Sync`. The plugin author writes standard async trait implementations with no awareness of the apartment boundary — the proxy and macro handle everything.

## Future Enhancements

### Tracing integration

`cdylib` plugins have their own copy of the `tracing` crate with a separate global subscriber (unset by default). This means `info!`, `debug!`, `warn!` etc. from plugin code are currently invisible to the host's logging infrastructure.

The `export_*!` macros already export a `hardy_create_runtime` symbol for the tokio runtime. The same pattern extends to tracing: the macro exports a `hardy_init_tracing` symbol that accepts a raw pointer to the host's `tracing::Dispatch`. The loader calls this symbol after loading, passing the host's dispatcher. The plugin's copy of `tracing` calls `tracing::dispatcher::set_global_default()` — setting it in the plugin's TLS so all subsequent log calls route to the host's subscriber.

This follows the same "plugin's code does the TLS write" principle used for the runtime: the host provides the value, but the plugin's copy of the crate performs the thread-local initialisation.

## Testing

- **ABI crate unit tests**: load test `.so`, verify ABI check, error handling for missing symbols, wrong token, and bad config.
- **CLA plugin integration**: see `tests/interop/mtcp/` — the MTCP/STCP CLA plugin exercises the full factory → register → forward → shutdown lifecycle.
- **Interop tests**: `tests/interop/ION/` and `tests/interop/ud3tn/` use the MTCP plugin against real DTN implementations in Docker.
- **ABI mismatch test**: build a plugin with a different `hardy-plugin-abi` version, verify the host rejects it with a clear error message.
