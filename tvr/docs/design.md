# hardy-tvr Design

Clock-driven Time-Variant Routing agent for Hardy DTN.

> Cross-ref: REQ-6 | Enables: DPP Gateway, Proactive Scheduling
>
> Related: [QoS/Bandwidth design](../../../dtn/docs/hardy/qos_bandwidth_design.md) |
> [Proactive Scheduling design](../../../dtn/docs/hardy/proactive_scheduling_design.md) |
> [DPP spec](../../../dpp/draft-taylor-dtn-dpp.md) |
> [Static Routes design](../../bpa-server/docs/static_routes_design.md) |
> [TVR Schedule YANG](../../references/draft-ietf-tvr-schedule-yang-08.txt)

## Design Goals

hardy-tvr exists to bridge the gap between *knowing when connectivity
will be available* and *having routes in the BPA at the right time*. The
existing `StaticRoutesAgent` handles permanent routes but has no concept
of time. Satellite passes, ground station windows, and scheduled
maintenance all require routes that appear and disappear on a clock.

The design goals were:

- **Source-agnostic ingestion.** Contact schedules come from many places:
  configuration files, DPP peering sessions, orchestration systems,
  operators with `grpcurl`. The scheduler should accept contacts from all
  of these through a single internal interface.

- **Operator-friendly file format.** Infrastructure operators already
  think in cron. The contact plan format is a deliberate superset of the
  static routes format, extended with cron recurrence, so there is no new
  syntax to learn and existing static route files work unchanged.

- **Correct DTN store-and-forward behaviour.** When a contact window
  closes and the route is withdrawn, bundles must return to `Waiting`
  state — not be dropped. This is fundamentally different from IP routing
  where no-route means unreachable. The `drop` action exists as an
  explicit policy override (e.g. scheduled maintenance) precisely because
  the default is wait.

- **Minimal BPA changes.** hardy-tvr uses only the existing
  `RoutingAgent`/`RoutingSink` API. No new traits, no proto changes, no
  BPA modifications.

## Architecture Overview

hardy-tvr is simultaneously a gRPC client (to the BPA) and a gRPC
server (exposing the TVR contact ingestion service). Internally it is
structured as three layers:

```
                                                  ┌──────────────────────────┐
  ┌────────────────┐                              │       hardy-tvr          │
  │  DPP Speaker   │──┐                           │                          │
  └────────────────┘  │  gRPC streaming           │  ┌───────────────────┐   │  routes
  ┌────────────────┐  │  sessions                 │  │   TVR Service     │   │──────────►┌─────┐
  │  Orchestrator  │──┤                           │  │   (gRPC server)   │   │ add_route │ BPA │
  └────────────────┘  │  Session open = register  │  └─────────┬─────────┘   │ remove_   │ RIB │
  ┌────────────────┐  │  Session close = cleanup  │            │             │ route     └─────┘
  │  grpcurl / CLI │──┘                           │  ┌─────────▼─────────┐   │
  └────────────────┘                              │  │    Scheduler      │   │
  ┌────────────────┐                              │  │    (clock loop)   │   │
  │ Contact Plan   │  file watch                  │  └───────────────────┘   │
  │ File           │─────────────────────────────►│  (internal, no session) │
  └────────────────┘                              └──────────────────────────┘
```

The **TVR gRPC service** accepts contact sessions from external clients.
The **file parser** reads the contact plan file and hot-reloads on
changes. Both feed into the **scheduler**, which maintains a timeline of
events and projects the currently-active time-slice into the BPA's
forwarding table via `add_route`/`remove_route` calls.

### RIB / FIB Split

hardy-tvr introduces a two-tier information model. The scheduler holds
the full RIB: all contacts with metadata, time windows, source labels,
and refcounts. The BPA holds the FIB: only the currently-active
forwarding entries (`pattern → action + priority`). The scheduler clock
sits at the RIB→FIB boundary, projecting the active slice.

> Note: The BPA's route table is named `RIB` in the codebase
> (`bpa/src/rib/`). In the TVR context it functions as a FIB — it holds
> only distilled forwarding entries, not full routing state.

### Relationship to DPP

hardy-tvr is the **Gateway** component in the DPP Speaker/Gateway split
(see DPP spec §2). The Speaker handles the control plane — peering
sessions, DNS identity, route advertisements, loop detection. hardy-tvr
handles the data plane — scheduling resolved contacts into routes.

The TVR gRPC service is the integration point between them. A DPP
Speaker translates `RouteAdvertisement` messages into `AddContacts` /
`RemoveContacts` calls. The `source` label per session provides
per-peer isolation. hardy-tvr has no awareness of distance-vector
routing concepts.

## Key Design Decisions

### Cron-based recurrence over YANG ietf-schedule

The IETF TVR Schedule YANG model (`draft-ietf-tvr-schedule-yang-08`)
uses `frequency` + `interval` + `count` for recurrence — a model
designed for YANG tooling. For a file-based contact plan, cron is more
natural: operators already know the syntax, and it directly expresses
"every weekday at 09:00" without converting to abstract frequency
intervals.

The cron engine supports 5-field (`min hr dom mon dow`) and 6-field
(`sec min hr dom mon dow`) expressions, named days and months
(`MON-FRI`, `MAR-OCT`), and shortcuts (`@daily`, `@hourly`). The
6-field form aligns with the YANG model's second-granularity durations.
See [`examples/contacts`](../examples/contacts) for the full file
format.

### Lazy recurrence expansion

Recurring contacts are not eagerly expanded into individual events — a
cron expression with no `until` would generate infinite events. Instead,
the scheduler computes only the next occurrence via
`CronExpr::next_after()`. When that occurrence's deactivation event
fires, the next pair is scheduled. At most two events per recurring
contact exist in the timeline at any time.

For contacts currently in an active occurrence at startup (detected via
`CronExpr::prev_before()`), the scheduler immediately installs the route
and schedules the deactivation for the remainder. This handles restarts
gracefully.

### Session-oriented gRPC over request/response

The TVR gRPC service uses bidirectional streaming sessions rather than
unary RPCs. This provides three properties that request/response cannot:

- **Identity binding.** The source identity is established once at
  session open, not self-declared per request. This prevents accidental
  cross-source interference.

- **Crash cleanup.** When a stream closes (client disconnect or crash),
  all contacts from that session are automatically withdrawn. No stale
  routes linger after a DPP Speaker restart.

- **Ownership isolation.** A client can only modify contacts within its
  own session. Combined with refcounting, this means withdrawing one
  source's contacts never affects another source's identical routes.

### Route refcounting

When multiple sources provide identical active contacts (same pattern,
action, priority), the scheduler calls `add_route` only once and
`remove_route` only when the last source withdraws. This prevents
route flapping when overlapping contact windows from different sources
cover the same destination.

### Synchronous route operations (backpressure)

Route install/remove operations are awaited sequentially in the
scheduler's core loop, not spawned as fire-and-forget tasks. If the BPA
is slow to process a route change, the scheduler stalls — which is the
correct behaviour. This provides backpressure rather than silently
dropping route operations, and guarantees ordering.

### Contact plan as superset of static routes

The contact plan file format is a deliberate superset of the static
routes format. A static route is simply a contact with no schedule
fields. This gives operators a migration path: start with static routes
in bpa-server, switch to hardy-tvr when schedules are needed, without
learning a new syntax. The in-process `StaticRoutesAgent` remains
valuable for simple deployments where the gRPC overhead of a separate
TVR process is unnecessary.

The only difference is that TVR excludes `reflect` — it is a diagnostic
action, not a scheduling concept. `drop` is supported as an explicit
policy override (e.g. maintenance windows) because DTN's default
no-route behaviour is wait/store, not drop.

### Bandwidth and delay as SI/humantime values

Link properties use human-readable units rather than raw numbers:
`bandwidth 10G` instead of `bps 10000000000`, and `delay 500ms` instead
of `delay 500000`. Bandwidth accepts SI suffixes (`K`, `M`, `G`, `T`,
with optional `bps` suffix, case-insensitive). Delay uses the
`humantime` format (`500ms`, `1s`, `250us`). Both are stored internally
as raw values (u64 bps, u32 microseconds) matching the proto and YANG
model.

These fields are informational — bandwidth enforcement requires
`PeerLinkInfo` on the CLA layer (see QoS design), which is not yet
implemented. The values are parsed and carried so the format is
forward-compatible.

## Time Basis

All schedule evaluation — cron matching, event firing, contact window
boundaries — operates in **UTC**. Cron expressions are matched against
UTC wall-clock time, so there are no daylight-saving transitions and
hours are never skipped or repeated. Timestamps in the contact plan
file that carry a non-UTC offset are converted to UTC at parse time
with a warning.

The underlying time model is **POSIX time** (Unix epoch seconds). This
applies to both the gRPC interface (`google.protobuf.Timestamp` is
defined as seconds since 1970-01-01T00:00:00Z with leap seconds
smeared) and the Rust `time::OffsetDateTime` used internally. Neither
counts leap seconds — every day is exactly 86400 seconds.

This means hardy-tvr's timestamps are **not TAI**. Systems that use
TAI or GPS time internally (common in flight dynamics and spacecraft
operations) must apply the UTC–TAI offset (currently 37 seconds) at
the ingestion boundary before submitting contacts via the gRPC service
or contact plan file. For contact windows measured in minutes or hours,
the difference is operationally negligible, but second-granularity
scheduling across time-scale boundaries requires explicit conversion.

## Integration

hardy-tvr integrates with Hardy through two interfaces:

- **RoutingAgent / RoutingSink** (BPA → TVR direction). hardy-tvr
  registers as a `RoutingAgent` with the BPA via `hardy-proto`. The BPA
  provides a `RoutingSink` through which hardy-tvr installs and withdraws
  routes. When the sink is dropped or `unregister()` is called, the BPA
  removes all routes from this agent.

- **TVR gRPC service** (external → TVR direction). Clients open
  streaming sessions to push contacts. The proto definition is in
  `tvr.proto`. Contact fields map to DPP `RouteAdvertisement` attributes
  and the IETF TVR Schedule YANG model.

The file parser and hot-reload watcher are internal sources that feed
the scheduler through the same interface as gRPC sessions, using
`"file:<path>"` as the source label.

## Standards Compliance

### REQ-6 (Contact Scheduling)

| ID | Requirement | Satisfied | Mechanism |
|----|-------------|-----------|-----------|
| 6.1 | Specify start of contact period | Yes | `start` field or cron expression |
| 6.2 | Specify duration of contact period | Yes | `end` field or `duration` with cron |
| 6.2a | Specify expected periodicity | Yes | Cron expression |
| 6.3 | Specify expected bandwidth | Partial | `bandwidth` field parsed; not enforced (PeerLinkInfo pending) |
| 6.4 | Updatable without restart | Yes | File hot-reload + TVR gRPC service |

### TVR Schedule YANG alignment

The cron engine's 6-field form provides second-granularity recurrence,
matching the YANG model's `duration` (uint32 seconds). Timestamps use
RFC 3339 (matching `yang:date-and-time`). The gRPC proto uses
`google.protobuf.Timestamp` and `google.protobuf.Duration` which
provide nanosecond precision throughout.

## Testing

See [unit_test_plan.md](unit_test_plan.md) for the full test plan with
requirements mapping and per-scenario coverage.
