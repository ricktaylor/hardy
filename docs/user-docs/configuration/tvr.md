# Time-Variant Routing (TVR)

The TVR agent installs and withdraws routes in the BPA on a schedule,
driven by contact plans from files, gRPC sessions, or both. It runs
as a separate process and connects to the BPA as a routing agent via
gRPC.

## Configuration

Configuration is read from YAML, TOML, or JSON files and environment
variables (`HARDY_TVR_` prefix, `__` for nesting).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `bpa-address` | URL string | `http://[::1]:50051` | BPA gRPC endpoint. |
| `agent-name` | String | `hardy-tvr` | Routing agent name registered with BPA. |
| `priority` | Integer | `100` | Default route priority for contacts without explicit priority. |
| `contact-plan` | File path | *(none)* | Path to contact plan file. If omitted, gRPC-only mode. |
| `watch` | Boolean | `true` | Monitor contact plan file for changes. |
| `grpc-listen` | Address | `[::1]:50052` | TVR gRPC service listen address. |
| `log-level` | `trace`, `debug`, `info`, `warn`, `error` | `info` | Logging verbosity. |

## Contact Plan File Format

The contact plan is a line-oriented text file. Each line defines a
contact with a pattern, an action, and optional fields in any order.
Lines starting with `#` are comments. Blank lines are ignored.

```
<pattern> <action> [fields...]
```

### Actions

- `via <eid>` — forward to next-hop EID
- `drop [<reason>]` — explicit discard with optional BPv7 reason code

### Schedule Types

**Permanent** (no schedule fields) — always active, equivalent to a
static route:

```
ipn:3.*.* via ipn:3.1.0 priority 10
```

**One-shot** (`start`/`end`) — active during a fixed time window:

```
ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z end 2026-03-27T09:30:00Z
```

**Recurring** (`cron` + `duration`) — repeating contact windows:

```
ipn:2.*.* via ipn:2.1.0 cron "0 8 * * *" duration 90m
ipn:5.*.* via ipn:5.1.0 cron "0 9 * * MON-FRI" duration 8h
```

### Time Basis

All timestamps and cron expressions are evaluated in **UTC**. There
are no daylight-saving transitions — hours are never skipped or
repeated.

Timestamps in the contact plan file may use any RFC 3339 offset (e.g.
`+05:30`), but they are converted to UTC at parse time with a warning.
gRPC timestamps (`google.protobuf.Timestamp`) are inherently UTC.

The time model is POSIX/Unix time — leap seconds are not counted.
Systems using TAI or GPS time must apply the UTC–TAI offset before
submitting contacts.

### Cron Expressions

Standard 5-field (`min hr dom mon dow`) or 6-field with seconds
(`sec min hr dom mon dow`). Supports `*`, `N`, `N-M`, `*/S`, `N-M/S`,
comma lists, named days (`SUN`-`SAT`), named months (`JAN`-`DEC`),
and shortcuts (`@daily`, `@hourly`, `@weekly`, `@monthly`, `@yearly`).

### Link Properties

- `bandwidth <rate>` — expected bandwidth with SI suffix: `256K`, `1M`,
  `10G`, `1T`, `256Kbps`, `10Gbps`, or bare number (bps)
- `delay <dur>` — expected one-way delay: `500ms`, `1s`, `250us`
- `priority <n>` — route priority (lower = checked first)

### Additional Fields

- `until <time>` — end of recurrence (RFC 3339)

## TVR gRPC Service

hardy-tvr exposes a session-oriented gRPC service on `grpc-listen` for
external contact sources (DPP Speakers, orchestration systems,
operators).

Each client opens a bidirectional streaming `Session()` RPC. The first
message must be `OpenSessionRequest` with a session name and default
priority. Subsequent messages add, remove, or replace contacts. When the
stream closes, all contacts from that session are automatically
withdrawn.

### Example with grpcurl

```bash
# Open a session and add a contact
grpcurl -plaintext -d @ [::1]:50052 tvr.Tvr/Session <<EOF
{"msg_id": 1, "open": {"name": "test", "default_priority": 100}}
{"msg_id": 2, "add": {"contacts": [{"pattern": "ipn:2.*.*", "via": "ipn:2.1.0"}]}}
EOF
```

## Hot-Reload

When `watch = true` (the default), hardy-tvr monitors the contact plan
file for changes using filesystem notifications with a 1-second
debounce window.

- **File modified**: re-parse and compute diff — new contacts are
  scheduled, removed contacts are withdrawn, unchanged contacts are
  unaffected.
- **File deleted**: all contacts from the file source are withdrawn.
- **Parse errors on reload**: logged, existing contacts kept.

## Metrics

When running with the `otel` feature and an OTLP exporter, the
following metrics are available:

| Metric | Type | Description |
|--------|------|-------------|
| `tvr_contacts` | gauge | Total managed contacts |
| `tvr_active_routes` | gauge | Routes currently installed in BPA |
| `tvr_timeline_depth` | gauge | Pending events in scheduler |
| `tvr_route_installs` | counter | Route install operations |
| `tvr_route_withdrawals` | counter | Route withdrawal operations |
| `tvr_sessions` | gauge | Active gRPC sessions |
| `tvr_file_reloads` | counter | File reload attempts (label: `outcome`) |
