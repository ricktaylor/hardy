# hardy-tvr — Time-Variant Routing Agent Design

> Cross-ref: REQ-6 | Depends: Routing Agent API (done) | Enables: DPP, Proactive Scheduling
>
> Related: [QoS/Bandwidth design](../../../dtn/docs/hardy/qos_bandwidth_design.md) |
> [Proactive Scheduling design](../../../dtn/docs/hardy/proactive_scheduling_design.md) |
> [DPP spec](../../../dpp/draft-taylor-dtn-dpp.md) |
> [Static Routes design](../../bpa-server/docs/static_routes_design.md) |
> [TVR Schedule YANG](../../references/draft-ietf-tvr-schedule-yang-08.txt)

## Purpose

hardy-tvr is a clock-driven RoutingAgent that receives scheduled contacts
and installs/withdraws routes in the BPA's RIB as contacts become active
and expire. It implements the **reactive route timing** path described in
the Proactive Scheduling design: routes are present only while their
contact window is open.

In DPP terms, hardy-tvr is the **Gateway** component — the local scheduling
engine that translates time-variant contact information into RIB operations.
It exposes a gRPC TVR service as the canonical input API, making it
source-agnostic: a contact plan file, a DPP Speaker, an orchestration
system, or an operator with `grpcurl` can all push contacts into the same
scheduler.

## Scope

**In scope:**

- gRPC TVR service for contact ingestion (the canonical input API)
- Contact plan file parsing as a built-in source (drives the same API)
- Clock-driven route installation and withdrawal via `RoutingSink`
- Contacts already active at startup
- File hot-reload with incremental diff
- Standalone binary: gRPC client to BPA, gRPC server for TVR service

**Out of scope (explicitly deferred):**

- Bandwidth/capacity-aware forwarding (lives on `PeerLinkInfo`, owned by CLA
  layer — see QoS design)
- Proactive scheduling / CGR (requires Contact Plan Store, bundle
  classification, deferred egress — see Proactive Scheduling design)
- Inter-domain route exchange (DPP peering protocol)
- Link establishment (CLA responsibility)
- Contact periodicity expansion (external tool responsibility)

## Architectural Fit

### Separation of Concerns

```
                    ┌──────────────────────────────────────────────┐
                    │                    BPA                        │
  Contact Plan      │                                              │
  ──────────────    │  ┌────────────────────────────────────────┐   │
  ipn:2.*.* via B   │  │  Routing Information Base (RIB)        │   │
  08:00–09:30       │  │                                        │   │
                    │  │  ┌──────────────┐ ┌─────────────────┐  │   │
  ┌──────────────┐  │  │  │ hardy-tvr    │ │ static_routes   │  │   │
  │  hardy-tvr   │──┼──│  │ (scheduled)  │ │ (permanent)     │  │   │
  │  (clock)     │  │  │  └──────────────┘ └─────────────────┘  │   │
  └──────────────┘  │  │  ┌──────────────┐                      │   │
                    │  │  │ CLA peers    │                      │   │
  ┌──────────────┐  │  │  │ (discovered) │                      │   │
  │ TCPCLv4 CLA  │──┼──│  └──────────────┘                      │   │
  │ (transport)  │  │  └────────────────────────────────────────┘   │
  └──────────────┘  └──────────────────────────────────────────────┘
```

| Concern | Owner | Mechanism |
|---------|-------|-----------|
| Route presence over time | hardy-tvr (RoutingAgent) | `add_route` / `remove_route` on clock |
| Physical link establishment | CLA (e.g. TCPCLv4) | TCP connections, TLS |
| Link properties (bandwidth, MTU) | CLA via `PeerLinkInfo` | Not yet implemented |
| Store-and-forward when no peer | BPA dispatcher | Bundles wait in `Waiting` state |
| Proactive "send now vs wait" | Future CGR / hardy-cgr | DEFERRED |

### RIB / FIB Split

hardy-tvr introduces a two-tier information model within its scope:

```
                       RIB                      FIB
               ┌─────────────────────┐    ┌──────────────────┐
               │  Resolved contacts  │    │  Active routes   │
               │                     │    │                  │
  DPP Speaker  │  - pattern          │    │  (pattern,       │
  ──(gRPC)────►│  - next-hop (Via)   │───►│   action,        │
  File ───────►│  - time windows     │    │   priority)      │
  CLI ────────►│  - source labels    │    │                  │
               │  - bandwidth        │    │  Used by BPA     │
               │  - refcounts        │    │  dispatcher for  │
               │                     │    │  forwarding      │
               └─────────────────────┘    └──────────────────┘
                     Scheduler                   BPA
                  (clock-driven)          (RoutingSink API)
```

| | TVR (RIB) | BPA (FIB) |
|---|---|---|
| **Contains** | All contacts with full metadata | Currently-active forwarding entries |
| **Indexed by** | Source + pattern + time window | Pattern + action + priority |
| **Knows about** | Time windows, bandwidth, source labels, refcounts | Nothing — just "match pattern, do action" |
| **Boundary** | Scheduler clock projects active contacts | BPA dispatcher looks up and forwards |

Contacts arrive at the TVR service already resolved: the `Contact` message
carries a concrete next-hop EID (not an unresolved `gateway_eid`), a
pattern, and a time window. Protocol-level concerns like AD_PATH, loop
detection, and sequence numbers belong to the DPP Speaker — they are
consumed and resolved there before contacts are pushed to the TVR service.
hardy-tvr has no awareness of distance-vector routing concepts.

The scheduler clock sits at the **RIB→FIB** boundary: it projects the
currently-active time-slice of the RIB into the BPA's forwarding table via
`add_route()` / `remove_route()` calls on the `RoutingSink`.

> Note: The BPA's route table is named `RIB` in the codebase (`bpa/src/rib/`).
> In the context of TVR the BPA's table functions as a FIB — it holds
> only distilled forwarding entries, not full routing state. This document
> uses FIB when referring to the BPA's table in the TVR context.

hardy-tvr uses only the existing `RoutingAgent` / `RoutingSink` API. No BPA
changes, no proto changes, no new traits.

When the RIB determines a contact becomes active:

```
hardy-tvr → sink.add_route(pattern, Via(next_hop), priority)
         → BPA installs entry in FIB
         → BPA re-evaluates Waiting bundles (poll_waiting_notify)
         → if CLA has peer connected: bundles flow
         → if no peer yet: bundles remain in Waiting state (correct)
```

When the RIB determines a contact has expired:

```
hardy-tvr → sink.remove_route(pattern, Via(next_hop), priority)
         → BPA removes entry from FIB
         → BPA re-evaluates ForwardPending bundles
         → bundles move to Waiting (await next contact or alternative route)
```

### Relationship to DPP

DPP defines two distinct roles (see DPP spec §2):

DPP Speaker:
: Participates in the DPP protocol — maintains peering sessions, exchanges
  route information, handles DNS-based identity verification. A control-plane
  role.

Gateway:
: A border node that forwards bundles to other Administrative Domains. A
  data-plane role. A gateway may or may not also be a DPP speaker.

This separation maps naturally onto Hardy's architecture:

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

hardy-tvr is the Gateway component. It exposes a **TVR gRPC service** as the
canonical API for contact ingestion. The built-in file parser is just another
client of the same internal interface — it reads the contact plan file and
feeds contacts into the scheduler exactly as a gRPC client would.

The TVR service is not DPP-specific. Any system that knows about future
connectivity can push contacts:

- **DPP Speaker** — translates inter-domain route advertisements into contacts
- **Orchestration systems** — temporospatial planners (e.g. Spacetime) that
  compute contact windows from antenna schedules, orbital mechanics, or
  link budgets
- **Mission planning tools** — export scheduled passes as contacts
- **Operators** — via `grpcurl` or a CLI tool for ad-hoc route injection

The `source` label on each request provides per-client isolation, so
contacts from different sources don't interfere with each other.

This makes hardy-tvr simultaneously:
- A **gRPC client** to the BPA (RoutingAgent via `routing.proto`)
- A **gRPC server** exposing the TVR service (contact ingestion)

The contact fields map to both DPP `RouteAdvertisement` attributes and
the IETF TVR Schedule YANG model (`draft-ietf-tvr-schedule-yang-08`):

| Contact field | DPP equivalent | TVR YANG equivalent | Hardy API mapping |
|---|---|---|---|
| pattern | `RouteAdvertisement.patterns` | — | `EidPattern` |
| via | `RouteAttribute.gateway_eid` | `destination-node` | `Action::Via(Eid)` |
| start/end | `valid_from` / `valid_until` | `period-start` / `period-end` | Timer → `add_route()` / `remove_route()` |
| cron + duration | — | `recurrence-utc` + `duration` | Lazy expansion → events |
| until | — | `utc-until` | Recurrence end condition |
| bandwidth | `bandwidth_bps` | `bandwidth` (gauge64) | Parsed, not enforced (future) |
| delay | — | `delay` (uint32, usec) | Parsed, not enforced (future) |
| priority | `metric` | — | `priority` parameter |

## Routing Actions in DTN Context

Unlike IP routing, where the absence of a route means **drop** (ICMP
unreachable), DTN's default is **store and wait**. When the BPA's FIB
has no matching route for a bundle, it returns `None` and the bundle
enters `Waiting` state — it is stored until a route appears.

This fundamentally changes the meaning of routing actions:

| Action | IP semantics | DTN semantics |
|--------|-------------|---------------|
| No route | Drop (unreachable) | **Wait** (store-and-forward) |
| Forward (`via`) | Forward to next-hop | Forward to next-hop |
| Drop | Blackhole | **Explicit discard** — override the default wait |

This means:
- When a contact's time window expires and its `via` route is removed,
  bundles are **not dropped** — they return to `Waiting` and will be
  forwarded when the next contact window opens. This is correct DTN
  store-and-forward behavior.
- A `drop` action is a deliberate policy decision: "during this window,
  actively discard bundles to this destination." It overrides the default
  wait-and-store. Use cases include maintenance windows, known-bad paths,
  or preventing bundle accumulation during extended outages.

Hardy's `Action` enum (`bpa/src/routes.rs`) has three variants:

```rust
pub enum Action {
    Drop(Option<ReasonCode>),   // Explicit discard
    Reflect,                     // Return to previous hop
    Via(Eid),                    // Forward via next-hop
}
```

For TVR, only `via` and `drop` are supported. `Reflect` is excluded — it
is a diagnostic/edge-case action that does not belong in contact
scheduling.

### Action in the File Format

The action keyword appears immediately after the pattern, following the
same convention as the static routes format:

```
<pattern> <action> [schedule] [properties]

where <action> is one of:
  via <eid>         Forward via next-hop EID
  drop [<reason>]   Drop with optional BPv7 reason code
```

## Contact Plan File Format

### Relationship to Static Routes

The contact plan format is a deliberate superset of the static routes
file format (see [Static Routes design](../../bpa-server/docs/static_routes_design.md)).
A static route is simply a contact with no schedule:

```
# Valid static route — also a valid contact plan entry
ipn:2.*.* via ipn:2.1.0 priority 10

# Same line, with a schedule added
ipn:2.*.* via ipn:2.1.0 priority 10 cron "0 8 * * *" duration 90m
```

The only difference is that TVR excludes `reflect` (static routes support
all three actions). Otherwise an existing static routes file can be handed
to hardy-tvr unchanged — every entry becomes a permanent route.

This gives operators a natural migration path: start with static routes in
bpa-server, switch to hardy-tvr when schedules are needed, without learning
a new syntax. The in-process `StaticRoutesAgent` remains valuable for
simple deployments where the gRPC overhead of a separate TVR process is
unnecessary.

### Syntax

Line-oriented, extended with time
windows and cron-style recurrence. The target audience — cloud/ground
infrastructure operators — already thinks in cron; a contact schedule is
conceptually a cron job for routes.

### One-shot Contacts

```
# Single Mars relay pass
ipn:2.*.* via ipn:2.1.0 start 2026-03-27T08:00:00Z end 2026-03-27T09:30:00Z bps 256000

# Permanent high-bandwidth ground link (no time window = always active)
ipn:3.*.* via ipn:3.1.0 priority 10
```

### Recurring Contacts

```
# Daily ground station pass at 08:00 UTC, 90 minutes, until end of campaign
ipn:2.*.* via ipn:2.1.0 cron "0 8 * * *" duration 90m until 2026-06-30T00:00:00Z bps 256000

# LEO relay: every 93 minutes, 12-minute pass window
ipn:4.*.* via ipn:4.1.0 cron "*/93 * * * *" duration 12m bps 1000000

# Weekday business hours only (backup terrestrial link)
ipn:5.*.* via ipn:5.1.0 cron "0 9 * * 1-5" duration 8h priority 50
```

### Scheduled Drop (Policy Override)

```
# Maintenance window: discard traffic to node every Sunday 02:00-06:00
# Priority 0 ensures this overrides any lower-priority via routes
ipn:6.*.* drop cron "0 2 * * 0" duration 4h priority 0

# Known outage: discard with reason code during planned downtime
ipn:7.*.* drop 3 start 2026-04-01T00:00:00Z end 2026-04-02T00:00:00Z priority 0
```

Note: outside the drop window, the default DTN behavior resumes — bundles
to `ipn:6.*.*` will wait for a matching `via` route, not be dropped.

### Cron Analogy

The `cron` keyword takes a standard 5-field cron expression
(`min hour dom month dow`). Each occurrence starts at the cron-matched
time and lasts for `duration`:

```
# crontab: run a job at 08:00 every day
0 8 * * *   /usr/bin/start-contact --duration 90m

# contact plan: route is live at 08:00 every day for 90 minutes
ipn:2.*.* via ipn:2.1.0 cron "0 8 * * *" duration 90m
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `<pattern>` | Yes | EID pattern (hardy-eid-patterns syntax) |
| `via <eid>` | Yes* | Forward via next-hop EID (`Action::Via`) |
| `drop [<reason>]` | Yes* | Explicit discard (`Action::Drop`), optional BPv7 reason code |
| `start <time>` | No | One-shot start (RFC 3339). If omitted: immediate |
| `end <time>` | No | One-shot end (RFC 3339). If omitted: until withdrawn |
| `cron "<expr>"` | No | Recurrence (5-field cron expression) |
| `duration <dur>` | With `cron` | How long each occurrence lasts (e.g. `90m`, `2h`, `4h30m`) |
| `until <time>` | No | End of recurrence (RFC 3339). If omitted: indefinite |
| `bps <rate>` | No | Expected bandwidth in bits/sec. Informational |
| `delay <usec>` | No | Expected one-way delay in microseconds. Informational |
| `priority <n>` | No | Route priority (default from config) |

\* Exactly one of `via` or `drop` is required.

A contact uses either `start`/`end` (one-shot) or `cron`/`duration`
(recurring). Both may be absent (permanent route). Contacts without any
time fields behave identically to static routes.

### Comments and Whitespace

Lines starting with `#` are comments. Blank lines are ignored.

## TVR gRPC Service

hardy-tvr exposes a session-oriented gRPC service for contact management.
Each client opens a bidirectional streaming session, providing:

- **Identity binding**: Source identity established at session open, not
  self-declared per request
- **Lifecycle management**: Stream close automatically withdraws all
  contacts from that session (crash cleanup)
- **Ownership**: A client can only modify contacts within its own session

This follows the same streaming pattern used by Hardy's other gRPC
interfaces (CLA, RoutingAgent, Service) and mirrors the long-lived
nature of DPP peering sessions.

### Service Definition

```protobuf
syntax = "proto3";
package dtn.tvr.v1;

import "google/protobuf/timestamp.proto";
import "google/protobuf/duration.proto";
import "google/rpc/status.proto";

service Tvr {
  // Open a contact session. The first message must be OpenSession.
  // Subsequent messages add, remove, or replace contacts within the
  // session. When the stream closes (client disconnect or crash),
  // all contacts from this session are automatically withdrawn.
  rpc Session(stream ClientMessage) returns (stream ServerMessage);
}

// ── Session Envelope ─────────────────────────────────────────────

message ClientMessage {
  uint32 msg_id = 1;
  oneof msg {
    OpenSessionRequest open = 2;
    AddContactsRequest add = 3;
    RemoveContactsRequest remove = 4;
    ReplaceContactsRequest replace = 5;
  }
}

message ServerMessage {
  uint32 msg_id = 1;
  oneof msg {
    google.rpc.Status status = 2;
    OpenSessionResponse open = 3;
    AddContactsResponse add = 4;
    RemoveContactsResponse remove = 5;
    ReplaceContactsResponse replace = 6;
  }
}

// ── Session Lifecycle ────────────────────────────────────────────

message OpenSessionRequest {
  string name = 1;                              // Session identity (e.g. "dpp:dsn.example.org")
  uint32 default_priority = 2;                  // Default priority for contacts without explicit override
}

message OpenSessionResponse {}

// ── Contact Operations ───────────────────────────────────────────

message Contact {
  string pattern = 1;                           // EID pattern
  optional uint32 priority = 2;                 // Override session default (needed for
                                                // patterns without monotonic specificity)

  oneof action {
    string via = 10;                            // Forward via next-hop EID
    DropAction drop = 11;                       // Explicit discard
  }

  oneof schedule {
    OneShot one_shot = 20;
    Recurring recurring = 21;
  }
  // If no schedule: permanent (always active, like a static route)

  optional uint64 bandwidth_bps = 30;            // Informational: bits/sec
  optional uint32 delay_us = 31;                 // Informational: one-way delay, microseconds
}

message DropAction {
  uint32 reason_code = 1;                       // Optional BPv7 reason code (0 = none)
}

message OneShot {
  google.protobuf.Timestamp start = 1;          // If absent: immediate
  google.protobuf.Timestamp end = 2;            // If absent: until withdrawn
}

message Recurring {
  string cron = 1;                              // 5-field cron expression
  google.protobuf.Duration duration = 2;        // Length of each occurrence
  google.protobuf.Timestamp until = 3;          // Optional: end of recurrence
}

message AddContactsRequest {
  repeated Contact contacts = 1;
}

message AddContactsResponse {
  uint32 added = 1;
  uint32 active = 2;                            // Immediately installed
  uint32 skipped = 3;                           // Past contacts ignored
}

message RemoveContactsRequest {
  repeated Contact contacts = 1;
}

message RemoveContactsResponse {
  uint32 removed = 1;
}

message ReplaceContactsRequest {
  repeated Contact contacts = 1;                // New complete set for this session
}

message ReplaceContactsResponse {
  uint32 added = 1;
  uint32 removed = 2;
  uint32 unchanged = 3;
}
```

Note: `source` is no longer a per-request field — it is bound to the
session via `OpenSessionRequest.name`. All operations within a session
implicitly belong to that source.

### Session Lifecycle

```
Client                                       hardy-tvr
   │                                             │
   │─── Session() RPC ──────────────────────────>│
   │<════════ Bidirectional Stream ═════════════>│
   │                                             │
   │─── OpenSession("dpp:dsn.example.org") ─────>│
   │<── OpenSessionResponse ─────────────────────│
   │                                             │
   │─── AddContacts([...]) ─────────────────────>│  Contacts scheduled
   │<── AddContactsResponse ─────────────────────│
   │                                             │
   │─── ReplaceContacts([...]) ─────────────────>│  Atomic diff
   │<── ReplaceContactsResponse ─────────────────│
   │                                             │
   │    (client crashes or disconnects)          │
   │────── stream closes ───────────────────────>│
   │                                             │  All contacts from
   │                                             │  this session withdrawn
```

Message correlation uses `msg_id` following the same pattern as Hardy's
other gRPC interfaces (see [proto design](../../proto/docs/design.md)).

### Route Ownership

hardy-tvr registers with the BPA as a single `RoutingAgent` (e.g. name
`"hardy-tvr"`). All routes it installs are attributed to that one source
in the BPA's FIB. The BPA sees one routing agent, one pool of routes.

Session identity is hardy-tvr's **internal** concept for isolating
contacts from different clients:

- **Per-session isolation**: A DPP Speaker's contacts don't interfere
  with an orchestrator's contacts
- **Crash cleanup**: Stream close withdraws only that session's contacts
- **Atomic replace**: `ReplaceContacts` diffs within a single session

If hardy-tvr itself crashes, the BPA removes all its routes at once —
correct behavior, since the scheduling engine is gone and all timing
decisions are stale.

If two sessions push identical contacts (same pattern, action, priority,
overlapping time window), the scheduler refcounts: only calls
`remove_route()` when the last session withdraws the contact.

### Priority and Specificity

The BPA's FIB selects routes by priority first (lower = checked first),
then by specificity score within a priority level (most specific pattern
wins). However, some EID patterns — particularly those with union
components (e.g. `ipn:[1-3].*.*`) — do not have a computable specificity
score. These patterns cannot be reliably ordered against other patterns
within the same priority level.

Per-contact priority is therefore needed for correctness, not just
convenience. The `OpenSessionRequest.default_priority` covers the common
case; `Contact.priority` overrides it for patterns that need explicit
positioning.

For the file format, `priority <n>` is optional per line. If absent, the
config-level default is used. The scheduler always resolves contacts to
concrete priorities before inserting into the RIB.

**Multi-source priority is an administrator responsibility.** When
multiple routing sources coexist (static routes, DPP peers, orchestration
systems), their relative priority is an operator decision — analogous to
admin distance between OSPF, BGP, and static routes in IP networking.
There is no safe way to auto-merge priorities across independent sources,
because each source has its own notion of "importance" that only the
operator can reconcile. The operator assigns session default priorities
to each source and uses per-contact overrides for edge cases.

### File Parser as Internal Source

The file parser is not a gRPC session — it calls the scheduler directly
with a synthetic source label (e.g. `"file:/etc/hardy-tvr/contacts"`).
On hot-reload, it calls the same `ReplaceContacts` logic internally.

```
File watcher → parse file → scheduler.replace("file:...", contacts)
                                      │
DPP Speaker ─── gRPC session ────────►│
                                      │
Orchestrator ── gRPC session ────────►│
                                      ▼
                                Scheduler (shared)
```

## Scheduler Design

### Event Model

The scheduler converts contacts into a sorted timeline of events:

```rust
enum Event {
    Add { pattern, action, priority },
    Remove { pattern, action, priority },
}

// Sorted by time, then Remove before Add (clean transition)
events: BTreeMap<OffsetDateTime, Vec<Event>>
```

### Recurrence and Lazy Expansion

Recurring contacts are not eagerly expanded into individual events — a
cron expression like `"*/93 * * * *"` with no `until` would generate
infinite events. Instead, the scheduler uses lazy expansion:

1. Compute the **next occurrence** from the cron expression
2. Generate `Add` and `Remove` events for that occurrence only
3. When the `Remove` event fires, compute the next occurrence and
   schedule its `Add`/`Remove` pair
4. Repeat until `until` is reached or the contact is withdrawn

This keeps the event queue bounded: at most two events per recurring
contact (one `Add`, one `Remove`) are in the queue at any time.

For contacts currently in an active occurrence at startup (cron says it
started N minutes ago, duration hasn't elapsed), the scheduler immediately
installs the route and schedules the `Remove` for the remainder.

### Core Loop

```
1. Ingest contacts → generate next events (lazy for recurring)
2. Loop:
   a. Sleep until next event time (tokio::time::sleep_until)
   b. Execute all events at that time
   c. For recurring: compute and schedule next occurrence
   d. Repeat
3. Wake on new contacts from gRPC or file reload
```

The loop also wakes when new contacts arrive (via a channel or notify),
so gRPC `AddContacts` calls take effect immediately without waiting for
the next scheduled event.

### Active at Startup

Contacts where `start <= now < end` (one-shot) or whose current cron
occurrence is in progress are active at startup. On registration:

1. Immediately `add_route()` for all currently-active contacts
2. Schedule `remove_route()` for their end times (or occurrence end)
3. Schedule future contacts/occurrences normally

This handles restarts gracefully: routes are restored without waiting for
the next contact window.

### Past Contacts

One-shot contacts where `end <= now` are silently skipped. Recurring
contacts whose `until` has passed are skipped. No warning, as this is
normal for a contact plan file that contains historical entries.

### Hot-Reload

Following the `StaticRoutesAgent` pattern:

1. File watcher (debounced, 1-second window)
2. On change: parse new file → `ReplaceContacts` into scheduler
3. Scheduler computes diff against current state:
   - Removed contacts: cancel pending events, `remove_route()` if active
   - Added contacts: schedule events, `add_route()` if currently active
   - Unchanged contacts: no action

### Permanent Routes

Contacts without any time fields generate a single `Add` event at "now"
and no `Remove` event. They are equivalent to static routes and persist
until the file is reloaded without them or a `RemoveContacts` call
withdraws them.

## Binary Structure

Standalone binary following the `tcpclv4-server` / `bpa-server` pattern:

```
tvr/
├── src/
│   ├── main.rs          # Entry point, config, signals
│   ├── config.rs         # Configuration structures
│   ├── parser.rs         # Contact plan file parser
│   ├── scheduler.rs      # Event timeline and clock loop
│   ├── agent.rs          # RoutingAgent trait implementation
│   └── server.rs         # TVR gRPC session service implementation
├── proto/
│   └── tvr.proto         # TVR service definition
├── docs/
│   └── design.md         # This document
└── Cargo.toml
```

### Configuration

```toml
[hardy-tvr]
contact-plan = "/etc/hardy-tvr/contacts"   # Optional: omit for gRPC-only mode
priority = 100
watch = true
grpc-listen = "[::1]:50052"

[grpc]
bpa-address = "http://[::1]:50051"
agent-name = "hardy-tvr"
```

| Option | Default | Description |
|--------|---------|-------------|
| `contact-plan` | None | Path to contact plan file. If omitted, no file source |
| `priority` | `100` | Default priority for contacts without explicit priority |
| `watch` | `true` | Monitor file for changes (only if `contact-plan` set) |
| `grpc-listen` | `[::1]:50052` | TVR service listen address |
| `bpa-address` | `http://[::1]:50051` | BPA gRPC endpoint |
| `agent-name` | `"hardy-tvr"` | Agent name registered with BPA (route source in RIB) |

Note: `contact-plan` is optional. hardy-tvr can run in gRPC-only mode,
receiving all contacts via streaming sessions (e.g. from a DPP Speaker).
Equally, it can run with only a file and no gRPC clients.

### Startup Sequence

1. Load config (TOML + env vars)
2. Connect to BPA via gRPC, register as RoutingAgent
3. On `on_register(sink, node_ids)`:
   a. Store sink
   b. Start scheduler task
4. Start TVR gRPC session server
5. If `contact-plan` configured:
   a. Parse file → replace into scheduler (source `"file:..."`)
   b. Start file watcher (if `watch` enabled)
6. Await SIGTERM / Ctrl+C
7. On shutdown: cancel token cascades to all sessions (contacts
   withdrawn), stop TVR server, `sink.unregister()`, exit

### Signal Handling

Following `tcpclv4-server` pattern: `TaskPool` cancellation on SIGTERM +
Ctrl+C. Hierarchical cancellation: server pool cancel → session child
tokens cancel → session contacts withdrawn → proxy reader/writer exit.
Unregister from BPA before exit for clean gRPC shutdown.

## REQ-6 Verification Matrix

| ID | Requirement | Satisfied | Mechanism |
|----|-------------|-----------|-----------|
| 6.1 | Specify start of contact period | Yes | `start` field or `cron` expression |
| 6.2 | Specify duration of contact period | Yes | `end` field or `duration` with cron |
| 6.2a | Specify expected periodicity of contact | Yes | `cron` expression (e.g. `"0 8 * * *"`) |
| 6.3 | Specify expected bandwidth during contact | Partial | `bps` field parsed; not enforced (PeerLinkInfo not yet implemented) |
| 6.4 | Updatable during deployment without restart | Yes | File hot-reload + TVR gRPC service |

Full bandwidth enforcement requires `PeerLinkInfo` on the CLA layer (see
QoS/Bandwidth design). hardy-tvr parses and logs bandwidth values so the
contact plan format is forward-compatible.

## Future Evolution

### Short-term: Bandwidth passthrough

When `PeerLinkInfo` is implemented on the CLA/peer layer, hardy-tvr could
optionally report bandwidth via a new `RoutingSink` method or via a
secondary CLA-side channel.

### Medium-term: DPP Speaker

The TVR gRPC service is the integration point. A DPP Speaker receives
`RouteAdvertisement` messages from external peers and translates them into
`AddContacts` / `RemoveContacts` calls on the TVR service. The Speaker
handles the DPP protocol complexity (DNS identity, handshake, keepalives,
loop detection, AD_PATH); hardy-tvr just schedules the resulting contacts.

The `source` label on each request (e.g. `"dpp:dsn.example.org"`) provides
per-peer isolation, so contacts from different DPP peers don't interfere
with each other or with file-based contacts.

### Long-term: Proactive scheduling

The Contact Plan Store described in the Proactive Scheduling design would
be populated from the same contact sources (file or DPP Speaker). Bundle
classification and CGR algorithms would use it for "send now vs wait"
decisions. hardy-tvr's reactive role remains unchanged — it handles route
timing; the proactive scheduler is a separate component that reads the same
contact data for forecasting.

## Open Questions

1. ~~**Should `drop` and `reflect` actions be supported?**~~ **Resolved**:
   `via` and `drop` are supported. `reflect` is excluded — it is a
   diagnostic action, not a scheduling concept. `drop` serves as an
   explicit policy override (e.g. scheduled maintenance windows) and is
   meaningful in DTN because the default no-route behavior is wait/store,
   not drop. See "Routing Actions in DTN Context" section.

2. ~~**Should the file format support relative times?**~~ **Resolved**:
   Cron-style recurrence covers the ergonomic need. One-shot contacts use
   RFC 3339 absolute times. Relative offsets (`+5m`) are not needed.

3. **ION contact plan format compatibility?** ION uses a different format
   (`a contact +<offset> +<offset> <from> <to> <rate>`). Supporting ION
   format as an alternative parser would aid migration. Could be a future
   addition without changing the scheduler.

4. **Workspace membership?** Should hardy-tvr be a workspace member (like
   bpa-server, tcpclv4-server) or workspace-excluded (like
   tests/interop/mtcp)? Since it's a first-class operational tool, workspace
   member seems appropriate.

5. ~~**Speaker→Gateway interface for DPP.**~~ **Resolved**: The TVR gRPC
   service is the canonical interface. The DPP Speaker calls `AddContacts`
   / `RemoveContacts` on the TVR service. The file parser uses the same
   internal API via `ReplaceContacts`.
