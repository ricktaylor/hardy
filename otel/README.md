# hardy-otel

OpenTelemetry metrics, tracing, and logging bridge for Hardy server binaries.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Installation

```toml
[dependencies]
hardy-otel = { version = "0.3", features = ["serde", "otlp"] }
```

Published on [crates.io](https://crates.io/crates/hardy-otel).

## Overview

hardy-otel sets up logging for Hardy binaries. `LogLevel` is the `log-level` key every binary's configuration carries (default `info`, overridden by `RUST_LOG`), and a single `init()` call installs the `tracing` subscriber at that level and returns an `OtelGuard` to hold until exit.

With the `otlp` feature, `init()` also wires the subscriber to OpenTelemetry trace and log exporters, and bridges the [`metrics`](https://docs.rs/metrics) crate facade to the OpenTelemetry SDK, so that Hardy binaries can emit metrics via standard Rust macros (`metrics::counter!`, `metrics::gauge!`, `metrics::histogram!`) and have them exported as OTLP telemetry. Without it, `init()` installs console logging only, and the OpenTelemetry stack is not compiled in.

Used by hardy-bpa-server, hardy-tcpclv4-server, and hardy-tvr, each of which turns `otlp` on through its own default `otel` feature, and by the mtcp interop CLA for logging alone.

## Features

- **`LogLevel`** -- parses `error`, `warn`, `info`, `debug`, `trace` (any case) or `1`-`5`; (de)serializes with the `serde` feature
- **One `init()` in both builds** -- console logging without `otlp`, the full export stack with it
- Feature flag: `serde` -- `Serialize`/`Deserialize` for `LogLevel`
- Feature flag: `otlp` -- the OpenTelemetry exporters and `metrics` recorder below

With `otlp`:

- **Gauge state tracking** -- increment, decrement, and set operations via
  atomic CAS, mapping the `metrics` push model to OTEL's record-based gauges
- **Counter forwarding** -- monotonic `u64` counters forwarded directly to
  OTEL
- **Histogram forwarding** -- `f64` observations forwarded directly to OTEL
- **Label propagation** -- `metrics` labels are mapped to OTEL `KeyValue`
  attributes; distinct label sets produce distinct instruments
- **UCUM unit mapping** -- human-readable `metrics::Unit` names (`seconds`,
  `bytes`, `count`) are translated to UCUM codes (`s`, `By`, `1`) per the
  OTEL specification
- **Lazy instrument creation** -- OTEL instruments are created on first use
  and cached in concurrent `DashMap`s
- **OTLP export** -- traces, metrics, and logs are exported over gRPC
  (tonic) to any OTLP-compatible collector
- **Telemetry loop suppression** -- internal OTEL and transport crate logs
  are filtered to prevent feedback loops

## Usage

```rust
// In your binary's main():
// `config.log_level` is a `hardy_otel::LogLevel`, read from the
// binary's configuration (`LogLevel::default()` is `info`).
let _guard = hardy_otel::init(
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_VERSION"),
    config.log_level,
);

// Now use metrics macros anywhere in your crate:
metrics::counter!("bundles.received").increment(1);
metrics::gauge!("bundles.pending", "reason" => "no_route").increment(1.0);
metrics::histogram!("dispatch.latency").record(0.042);

// With `otlp`, OtelGuard flushes pending telemetry on drop.
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [Changelog](CHANGELOG.md)
- [API Documentation](https://docs.rs/hardy-otel)
- [User Documentation](https://ricktaylor.github.io/hardy/operations/observability/)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
