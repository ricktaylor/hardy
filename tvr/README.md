# Hardy TVR

Time-Variant Routing agent for Hardy DTN. Installs and withdraws routes
in the BPA on a clock, driven by contact schedules from files, gRPC
sessions, or both.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Quick Start

1. Create a contact plan file:

   ```
   # Ground station pass — daily at 08:00 UTC, 90 minutes
   ipn:2.*.* via ipn:2.1.0 cron "0 8 * * *" duration 90m bandwidth 256K

   # Permanent backbone link
   ipn:3.*.* via ipn:3.1.0 priority 10 bandwidth 10G
   ```

2. Configure hardy-tvr (`hardy-tvr.toml`):

   ```toml
   bpa-address = "http://[::1]:50051"
   contact-plan = "/etc/hardy-tvr/contacts"
   ```

3. Run:

   ```
   hardy-tvr --config hardy-tvr.toml
   ```

Routes appear in the BPA when contact windows open and are withdrawn
when they close. Bundles waiting for a destination are automatically
re-evaluated when routes appear.

## Configuration

Configuration is read from TOML files and environment variables
(`HARDY_TVR_` prefix). By default, `hardy-tvr.toml` is read from the
current directory.

| Option | Default | Description |
|--------|---------|-------------|
| `bpa-address` | `http://[::1]:50051` | BPA gRPC endpoint |
| `agent-name` | `hardy-tvr` | Routing agent name in BPA |
| `priority` | `100` | Default route priority for contacts without explicit priority |
| `contact-plan` | *(none)* | Path to contact plan file. If omitted, gRPC-only mode |
| `watch` | `true` | Monitor contact plan file for changes |
| `grpc-listen` | `[::1]:50052` | TVR gRPC service listen address |
| `log-level` | `error` | Logging level (`trace`, `debug`, `info`, `warn`, `error`) |

Environment variables override file settings using `HARDY_TVR_` prefix
with underscores (e.g. `HARDY_TVR_BPA_ADDRESS`).

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

See [`examples/contacts`](examples/contacts) for a comprehensive
example.

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

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/bpa-server/#routing)

## Licence

Apache 2.0 — see [LICENSE](../LICENSE)
