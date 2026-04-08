# Observability

Hardy integrates with [OpenTelemetry](https://opentelemetry.io/) for
metrics, traces, and structured logging. Telemetry data is exported
over OTLP/gRPC to any compatible collector.

## Quick Start

Start a local Grafana stack with an OpenTelemetry collector:

```bash
docker run -p 3000:3000 -p 4317:4317 -p 4318:4318 --rm -ti grafana/otel-lgtm
```

This provides:

- **Grafana** at [http://localhost:3000](http://localhost:3000) (visualisation)
- **OTLP/gRPC** on port 4317 (traces, metrics, logs)
- **OTLP/HTTP** on port 4318 (alternative)
- **Loki** for log storage, **Tempo** for traces, **Mimir** for metrics

Then start Hardy — it will automatically export telemetry to
`localhost:4317`.

## Configuration

OpenTelemetry is enabled by default. When an OTLP collector is
reachable, Hardy automatically exports logs, traces, and metrics.

### OTLP Endpoint

The collector address is configured via the standard OpenTelemetry
environment variable:

| Variable | Description | Default |
|----------|-------------|---------|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP collector address. | `http://localhost:4317` |
| `OTEL_SERVICE_NAME` | Service name reported in telemetry. | Binary name |

### Log Levels

| Level | Description |
|-------|-------------|
| `error` | Errors only (default). |
| `warn` | Warnings and errors. |
| `info` | Informational messages, warnings, and errors. |
| `debug` | Debug output including internal state changes. |
| `trace` | Most verbose — all internal details including per-bundle processing. |

See [BPA Server Configuration](../configuration/bpa-server.md#top-level-options)
for how to set the log level.

## Logging

Hardy uses structured logging. Log output goes to two destinations:

1. **Console** — human-readable formatted output to stderr.
2. **OTLP** — structured log records exported to the collector.

Logs include structured fields such as bundle IDs, peer addresses, and
operation durations. In Grafana, use the Loki data source to query and
filter logs.

## Traces

Hardy emits distributed traces covering bundle processing operations.
Each trace represents the lifecycle of a bundle through the BPA
pipeline: ingestion, validation, filtering, dispatch, and egress.

Traces are viewable in Grafana via the Tempo data source.

## Metrics

Metrics are exported every 60 seconds to the collector.

### BPA Metrics

!!! note "TODO"
    BPA metrics table — pending merge of metrics PR.

### TVR Metrics

When the [TVR agent](https://github.com/ricktaylor/hardy/blob/main/tvr/README.md)
is deployed, the following additional metrics are available:

| Metric | Type | Description |
|--------|------|-------------|
| `tvr_contacts` | Gauge | Total managed contacts. |
| `tvr_active_routes` | Gauge | Routes currently installed in the BPA. |
| `tvr_timeline_depth` | Gauge | Pending events in the scheduler. |
| `tvr_route_installs` | Counter | Route install operations. |
| `tvr_route_withdrawals` | Counter | Route withdrawal operations. |
| `tvr_sessions` | Gauge | Active gRPC sessions. |
| `tvr_file_reloads` | Counter | File reload attempts (labelled by `outcome`). |

## Production Collector

The [Quick Start](#quick-start) above uses an all-in-one Grafana stack
for development. For production deployments, run a standalone
OpenTelemetry Collector with your preferred backends and set the
endpoint:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.internal:4317
```

## Disabling Telemetry

If no OTLP collector is reachable, Hardy continues to operate normally
— telemetry export fails silently and logging to the console is
unaffected.
