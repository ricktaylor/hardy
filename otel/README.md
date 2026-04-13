# hardy-otel

OpenTelemetry metrics, tracing, and logging bridge for Hardy server binaries.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

hardy-otel bridges the [`metrics`](https://docs.rs/metrics) crate facade to
the OpenTelemetry SDK, so that Hardy binaries can emit metrics via standard
Rust macros (`metrics::counter!`, `metrics::gauge!`, `metrics::histogram!`)
and have them exported as OTLP telemetry. It also wires up the `tracing`
subscriber with OpenTelemetry trace and log exporters in a single `init()`
call.

Used by hardy-bpa-server, hardy-tcpclv4-server, and hardy-tvr -- any Hardy
binary that exports OTEL telemetry.

## Features

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
let _guard = hardy_otel::init(
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_VERSION"),
    tracing::Level::INFO,
);

// Now use metrics macros anywhere in your crate:
metrics::counter!("bundles.received").increment(1);
metrics::gauge!("bundles.pending", "reason" => "no_route").increment(1.0);
metrics::histogram!("dispatch.latency").record(0.042);

// OtelGuard flushes pending telemetry on drop.
```

## Documentation

- [API Documentation](https://docs.rs/hardy-otel)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
