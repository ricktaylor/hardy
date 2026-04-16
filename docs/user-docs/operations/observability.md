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

#### Bundle Processing

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bpa.bundle.received` | counter | | Bundles received from CLAs |
| `bpa.bundle.received.bytes` | counter | | Total bytes received |
| `bpa.bundle.received.dropped` | counter | | Bundles dropped during ingress (validation, filter, expiry) |
| `bpa.bundle.received.duplicate` | counter | | Duplicate bundles detected |
| `bpa.bundle.originated` | counter | | Bundles originated by local services |
| `bpa.bundle.originated.bytes` | counter | | Total bytes originated |
| `bpa.bundle.forwarded` | counter | | Bundles successfully forwarded to CLAs |
| `bpa.bundle.forwarding.failed` | counter | | Forward attempts that failed |
| `bpa.bundle.delivered` | counter | | Bundles delivered to local services |
| `bpa.bundle.dropped` | counter | `reason` | Bundles deleted (labelled by reason code) |
| `bpa.bundle.reassembled` | counter | | ADUs successfully reassembled from fragments |
| `bpa.bundle.reassembly.failed` | counter | | Reassembly failures |
| `bpa.bundle.status` | gauge | `state` | Current bundles by processing state (new, dispatching, forward_pending, waiting, etc.) |

#### Status Reports

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bpa.status_report.sent` | counter | `type` | Status reports generated (reception, forwarding, delivery, deletion) |
| `bpa.status_report.received` | counter | `type` | Status reports received from peers |
| `bpa.admin_record.received` | counter | | Administrative records received |
| `bpa.admin_record.unknown` | counter | | Unrecognised administrative record types |

#### Routing (RIB)

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `bpa.rib.agents` | gauge | | Registered routing agents |
| `bpa.rib.entries` | gauge | `source` | Route entries by source agent |

#### Storage

| Metric | Type | Description |
|--------|------|-------------|
| `bpa.store.cache.hits` | counter | LRU cache hits |
| `bpa.store.cache.misses` | counter | LRU cache misses |
| `bpa.store.cache.oversized` | counter | Bundles too large for LRU cache |
| `bpa.mem_store.bundles` | gauge | In-memory bundle store entry count |
| `bpa.mem_store.bytes` | gauge | In-memory bundle store total bytes |
| `bpa.mem_store.evictions` | counter | LRU evictions from in-memory store |
| `bpa.mem_metadata.entries` | gauge | In-memory metadata store entries |
| `bpa.mem_metadata.tombstones` | gauge | In-memory metadata store tombstones |

#### Restart Recovery

| Metric | Type | Description |
|--------|------|-------------|
| `bpa.restart.lost` | counter | Bundles lost during recovery (missing from both stores) |
| `bpa.restart.duplicate` | counter | Duplicate bundles found during recovery |
| `bpa.restart.orphan` | counter | Bundle data without matching metadata |
| `bpa.restart.junk` | counter | Unreadable data cleaned up during recovery |

### TCPCLv4 Metrics

When TCPCLv4 is enabled (embedded or standalone), the following metrics
are available:

#### Sessions

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tcpclv4.session.established` | counter | | Sessions successfully established |
| `tcpclv4.session.terminated` | counter | `reason` | Sessions terminated (by reason code, hangup, codec error, or I/O error) |

#### Transfers

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `tcpclv4.transfers.sent` | counter | | Complete bundles forwarded to peers |
| `tcpclv4.transfers.received` | counter | | Complete bundles received from peers |
| `tcpclv4.transfers.refused` | counter | `reason` | Transfers refused by peer (by refuse reason code) |
| `tcpclv4.segments.sent` | counter | | XFER_SEGMENT messages sent |
| `tcpclv4.segments.received` | counter | | XFER_SEGMENT messages received |

#### Throughput

| Metric | Type | Description |
|--------|------|-------------|
| `tcpclv4.session.bytes.sent` | counter | Total bytes written to TCP connections |
| `tcpclv4.session.bytes.received` | counter | Total bytes read from TCP connections |

#### Connection Pool

| Metric | Type | Description |
|--------|------|-------------|
| `tcpclv4.pool.idle` | gauge | Idle connections available for reuse |
| `tcpclv4.pool.reused` | counter | Connections reused from the idle pool |

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
