# hardy-otel Design

Consolidated OpenTelemetry integration providing logs, traces, and metrics export for all Hardy applications.

## Design Goals

- **Single telemetry library.** Provide one library that all Hardy applications use for observability. This ensures consistent telemetry configuration across the project and avoids duplicating OpenTelemetry setup code in each application.

- **Unified observability.** Connect all three telemetry signals (logs, traces, metrics) to an OpenTelemetry collector. Operators get a consistent view of system behaviour through standard tooling.

- **Hide complexity.** OpenTelemetry's Rust ecosystem involves multiple crates with non-trivial configuration. This library encapsulates the complexity into a single `init()` function.

- **Guard-based lifecycle.** Telemetry resources must be flushed and shut down properly. The `OtelGuard` ensures providers are cleaned up when the application exits.

## Architecture Overview

The library bridges Rust's telemetry crates to OpenTelemetry exporters:

```
Rust Crates                    OpenTelemetry
───────────                    ─────────────
tracing events ──► OpenTelemetryTracingBridge ──► LogExporter ──┐
tracing spans  ──► OpenTelemetryLayer ──────────► SpanExporter ─┼──► OTLP/gRPC
metrics facade ──► OpenTelemetryRecorder ───────► MetricExporter┘
```

Each signal type has its own SDK provider:
- **SdkTracerProvider** - Span collection and export
- **SdkMeterProvider** - Metrics aggregation and export
- **SdkLoggerProvider** - Log record export

All providers use OTLP export over gRPC (via tonic) to a collector at `localhost:4317` by default, configurable via `OTEL_EXPORTER_OTLP_ENDPOINT`.

## Key Design Decisions

### Single init() Entry Point

Rather than requiring applications to configure each provider separately, `init()` sets up all three signal types with sensible defaults. It returns an `OtelGuard` that the caller must keep alive; dropping the guard triggers provider shutdown with proper flushing.

This trades flexibility for simplicity - most Hardy applications need identical telemetry configuration.

### tracing as the Instrumentation Framework

Hardy uses the `tracing` crate exclusively for instrumentation. It provides both structured logging (events) and span-based tracing in a single API. The `tracing` macros (`info!`, `debug!`, `#[instrument]`) are used throughout the codebase.

The library connects `tracing` to OpenTelemetry in two ways:
- **OpenTelemetryTracingBridge** - Routes tracing events to the log exporter
- **OpenTelemetryLayer** - Exports spans to the trace exporter

### metrics Crate Facade

For metrics, Hardy uses the `metrics` crate facade. The `OpenTelemetryRecorder` implements `metrics::Recorder`, translating `metrics!` macro calls to OpenTelemetry instrument operations.

Instruments are lazily created and cached using `DashMap` for thread-safe concurrent access. Counter, gauge, and histogram types are supported.

### Telemetry Loop Prevention

A common problem with OpenTelemetry integration is telemetry-induced-telemetry: the OTLP exporter uses HTTP/gRPC, which generates its own logs and traces, which get exported, creating an infinite loop.

The library filters out logs from crates used by the exporters:
- `reqwest`, `tonic`, `tower`, `h2` - HTTP/gRPC stack
- `opentelemetry` - Internal OpenTelemetry logs (on console output only)

This filtering is applied via `EnvFilter` directives. The trade-off is that logs from these crates are suppressed even when used outside the exporter context.

### Batch vs Periodic Export

Export strategies differ by signal type:
- **Traces and logs** use batch export - events accumulate and are sent in batches to reduce overhead
- **Metrics** use periodic export - aggregated values are sent at regular intervals

## Integration

### With hardy-bpa-server

When compiled with the `otel` feature, the server calls `hardy_otel::init()` at startup and holds the guard for its lifetime.

### With hardy-tcpclv4-server

Similar integration for the standalone TCPCLv4 server.

## Dependencies

| Crate | Purpose |
|-------|---------|
| opentelemetry | Core OpenTelemetry API |
| opentelemetry-sdk | Provider implementations |
| opentelemetry-otlp | OTLP/gRPC exporters |
| tracing-opentelemetry | Span export bridge |
| opentelemetry-appender-tracing | Log export bridge |
| tracing-subscriber | Subscriber configuration and filtering |
| metrics | Metrics facade |
| dashmap | Thread-safe instrument caching |
