# Policy Subsystem Design

This document describes the egress policy subsystem in the BPA, covering flow classification, queue management, and the integration with CLA forwarding.

## Related Documents

- **[Routing Design](routing_subsystem_design.md)**: RIB lookup and peer selection (ECMP uses flow_label)
- **[Bundle State Machine Design](bundle_state_machine_design.md)**: `ForwardPending { peer, queue }` status
- **[Filter Subsystem Design](filter_subsystem_design.md)**: Ingress filters can set flow_label
- **[Storage Subsystem Design](storage_subsystem_design.md)**: Hybrid channel implementation and bundle persistence

## Overview

The policy subsystem controls **how** and **when** bundles are transmitted to peers. It provides:

| Aspect | Purpose |
|--------|---------|
| **Flow Classification** | Map flow_label to queue index |
| **Queue Management** | Priority-based queue hierarchy |
| **Rate Control** | Backpressure and rate limiting via controllers |
| **CLA Integration** | Policy wraps CLA transmission |

## Architecture Diagram

```
┌─────────────────────────────────────────────────────────────────────┐
│                         Dispatcher                                  │
│                                                                     │
│  process_bundle() → RIB::find() → FindResult::Forward(peer_id)      │
│                                                                     │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                       CLA Registry                                  │
│                                                                     │
│  forward(peer_id, bundle) → PeerTable lookup                        │
│                                                                     │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                          Peer                                       │
│                                                                     │
│  1. Extract flow_label from bundle.metadata.writable.flow_label     │
│  2. policy.classify(flow_label) → queue index                       │
│  3. Send to queue channel                                           │
│                                                                     │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                  │
│  │  Queue 0    │  │  Queue 1    │  │  Queue None │                  │
│  │  (highest)  │  │  (medium)   │  │  (best-eff) │                  │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘                  │
│         │                │                │                         │
└─────────┼────────────────┼────────────────┼─────────────────────────┘
          │                │                │
          ▼                ▼                ▼
┌─────────────────────────────────────────────────────────────────────┐
│                    Queue Pollers (per queue)                        │
│                                                                     │
│  recv_async() → controller.forward(queue, bundle)                   │
│                                                                     │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      EgressController                               │
│                                                                     │
│  Policy enforcement: rate limiting, scheduling, etc.                │
│  Calls egress_queue.forward(bundle)                                 │
│                                                                     │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                   Dispatcher.forward_bundle()                       │
│                                                                     │
│  1. Load bundle data                                                │
│  2. Update extension blocks (Hop Count, Previous Node, Bundle Age)  │
│  3. Run Egress filters (see filter_subsystem_design.md)             │
│  4. CLA.forward(queue, cla_addr, data)                              │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
```

## Policy Traits

The policy subsystem defines three traits. See rustdoc for full API details.

**EgressPolicy** is the main policy interface that defines queue count, flow classification logic, and creates controllers for peer queue sets.

**EgressController** is the policy enforcement point created per-peer. It mediates all `forward(queue, bundle)` calls, allowing rate limiting and scheduling.

**EgressQueue** is the transmission endpoint that forwards bundles through the dispatcher to the CLA.

## Flow Classification

### Flow Label Source

The `flow_label` is stored in bundle metadata (`WritableMetadata`) and can be set by Ingress filters. See [Filter Subsystem Design](filter_subsystem_design.md) for filter implementation details.

### Classification Process

When a bundle reaches a peer for forwarding, the policy's `classify()` method maps the flow_label to a queue index. Bundles without a flow_label default to the best-effort queue (`None`). If classification returns an invalid queue index, it falls back to the best-effort queue.

### ECMP Peer Selection

Flow labels also affect peer selection during routing. See [Routing Design](routing_subsystem_design.md) for details.

When multiple peers can reach a destination, the RIB uses a hash of bundle source, destination, and flow_label for deterministic peer selection. This ensures bundles with the same flow_label always route to the same peer, preventing out-of-order delivery.

## Queue Management

### Queue Hierarchy

| Queue | Priority | Purpose |
|-------|----------|---------|
| `Some(0)` | Highest | Critical traffic |
| `Some(1)` | Medium | Standard traffic |
| `Some(n)` | Decreasing | Lower priority |
| `None` | Lowest | Best-effort / fallback |

### Strict Priority Semantics

From `src/cla/mod.rs`:

> If a CLA implements more than one queue, it is expected to implement strict priority. This means it will always transmit all packets from the highest priority queue (e.g., Queue 0) before servicing the next one (Queue 1), ensuring minimal latency for critical traffic.

### Queue Creation

For each peer, queues are created based on the policy's `queue_count()`. A best-effort queue (`None`) is always created, plus numbered priority queues `Some(0)` through `Some(queue_count-1)`.

### Queue Pollers

Each queue has a dedicated background task that receives bundles from the channel and forwards them through the controller. The bundle status `ForwardPending { peer, queue }` persists the queue assignment for crash recovery. See [Bundle State Machine Design](bundle_state_machine_design.md).

## Hybrid Channel Architecture

The queue channels implement a fast/slow path hybrid for backpressure (`src/storage/channel.rs`):

```
┌──────────────────────────────┐
│           Open               │  Fast path: direct memory channel
│    (try_send succeeds)       │
└──────────────┬───────────────┘
               │ channel full
               ▼
┌──────────────────────────────┐
│         Draining             │  Slow path: poll from storage
│   (poller drains storage)    │
└──────────────┬───────────────┘
               │ new arrivals during drain
               ▼
┌──────────────────────────────┐
│         Congested            │  Continue draining, then re-open
│    (work queued in storage)  │
└──────────────────────────────┘
```

### Fast Path

- Direct channel send
- Sub-millisecond latency
- Capacity: `poll_channel_depth` configuration

### Slow Path

- Bundle stored in persistent metadata with `ForwardPending` status
- Background poller drains storage to channel
- Automatic backpressure via storage insertion rate
- Hysteresis: requires <50% channel utilization to re-open fast path

## CLA Integration

### Policy Configuration

Policies are defined as named, reusable configurations in the bpa-server
config. Each policy specifies a type (qdisc) and its parameters. CLAs
reference a policy by name at registration time.

```toml
[policies.satellite]
type = "htb"
# class hierarchy, rates, borrowing rules...

[policies.ground-link]
type = "strict-priority"
classes = 4

[policies.best-effort]
type = "null"

[cla.tcpclv4]
policy = "satellite"

[cla.udpcl]
policy = "ground-link"
```

The policy definition describes *what* the scheduling should be (traffic
classes, priorities, rates). The controller adapts *how* to map those
classes onto the CLA's actual queues at peer establishment time. The same
named policy works regardless of the CLA's queue count — the controller
degrades gracefully (multiple classes share a queue when N < M, or
classes spread across queues when N > M).

Multiple CLAs can reference the same named policy. A CLA that registers
without specifying a policy (e.g., a gRPC-connected CLA server) defaults
to the null policy.

This follows the linux `tc` model: the policy config defines the class
hierarchy (like `tc class add`), and the controller is the qdisc
attached to the device (CLA). The qdisc queries the device for
capabilities (queue count) and builds the class-to-queue mapping at
attach time, not at config time.

### CLA Queue Count

CLAs declare their queue count via `queue_count()` (default: 0, meaning
single best-effort queue). These are physical or logical transport
channels — QUIC streams, DSCP classes, separate TCP connections. The
queue index is passed to `CLA::forward()`.

The CLA does not need to understand traffic classes. It implements strict
priority across its own queues (queue 0 before queue 1) and transmits
what the controller gives it. All traffic class intelligence lives in
the BPA's policy controller.

### Runtime Class-to-Queue Mapping

The policy config defines M traffic classes. The CLA advertises N queues
at registration time. When a peer connects, the controller builds the
M→N mapping:

**N >= M** — each class gets its own CLA queue. Remaining CLA queues
are unused (or classes can be spread for isolation). Scheduling within
each queue is FIFO since each class is already separated.

**N < M** — multiple classes share CLA queues. The controller does
priority scheduling within each shared queue, ensuring higher-priority
classes are serviced before lower-priority ones on the same output.

**N = 1** — all classes collapse onto a single CLA queue. The controller
does all scheduling internally, feeding one output stream in priority
order. The CLA sees a single FIFO, but the bundle ordering reflects
the full HTB discipline.

**N = 0** — equivalent to N = 1 (single best-effort queue, `None`).

The mapping is built once at controller creation and is fixed for the
lifetime of the peer connection. If the CLA's capabilities change (e.g.,
reconfiguration), the peer must be re-established to pick up the new
mapping.

### Controller Lifecycle

1. **Peer connects**: Policy creates controller via `new_controller(queues)`.
   The controller receives the actual CLA queue set and builds its
   class-to-queue mapping based on the CLA's capabilities
2. **Bundle forwarding**: Controller mediates all `forward(class, bundle)`
   calls, scheduling across traffic classes and mapping to CLA queues
3. **Peer removal**: Controller dropped, pollers exit naturally

## Default Policy (Null Policy)

The null policy provides simple FIFO behavior: it declares zero priority queues, classifies all bundles to the best-effort queue (`None`), and its controller simply passes bundles through without rate limiting or scheduling.

### Known wart: `Option<u32>` queue indices

CLA queue indices are currently `Option<u32>`, where `None` means "the
default queue." This was a natural fit for the null policy (everything is
`None`) but creates an awkward special case: `None` doesn't mean "no
queue" — it means queue 0 by another name. The `queue_count()` return
value of 0 means "one queue (the `None` queue)," which is confusing.

A cleaner model would use plain `u32` indices starting at 0. The default
/ best-effort queue would just be a numbered queue (e.g., the highest
index, or whichever the policy designates). `queue_count()` would return
the actual count (1 for a single-queue CLA). No special casing for
`None` vs `Some(0)`.

This cleanup should be done when implementing a real policy — it touches
the CLA trait, peer queue maps, `ForwardPending` status, storage
channels, and tests, so it's not worth the churn until there's a
functional reason to make the change.

## Three-Stage Egress Pipeline

The egress path has three conceptually distinct stages. The current null
policy collapses all three, but an advanced policy implementation must
separate them.

### Stage 1: Label (Filter)

Ingress or originate filters tag bundles with a `flow_label: Option<u32>`
in `WritableMetadata`. The label is an opaque application-level identifier
— it carries no scheduling semantics itself. Examples: DSCP value from
the payload, application priority, mission phase identifier.

### Stage 2: Classify (EgressPolicy)

`EgressPolicy::classify(flow_label) → traffic_class` maps the label to
one of M traffic classes. Traffic classes carry scheduling semantics:
priority level, rate guarantees, burst allowances. The number of traffic
classes (M) is independent of the number of CLA queues (N).

**Current limitation:** `classify` returns `Option<u32>` which is treated
as a CLA queue index, conflating traffic class with CLA queue. An HTB
implementation would need `classify` to return a traffic class that the
controller then maps to CLA queues.

### Stage 3: Schedule (EgressController)

The `EgressController` is the scheduler. It accepts bundles tagged with
traffic classes and multiplexes M classes onto N CLA queues using a
scheduling discipline (HTB, WFQ, strict priority, etc.).

The controller is the only component that knows both:

- The traffic class semantics (M classes with priorities and rates)
- The CLA's capabilities (N queues, link bandwidth)

It performs the M→N mapping, ensuring high-priority classes get
preferential access to CLA queue capacity while low-priority classes can
borrow when capacity is available.

### Pipeline diagram

```
Filter              EgressPolicy         EgressController        CLA
  │                     │                       │                  │
  │  flow_label (u32)   │                       │                  │
  ├────────────────────►│                       │                  │
  │                     │  traffic_class        │                  │
  │                     ├──────────────────────►│                  │
  │                     │                       │  HTB scheduling  │
  │                     │                       │  M classes → N   │
  │                     │                       │  CLA queues      │
  │                     │                       ├─────────────────►│
  │                     │                       │  forward(q, data)│
```

### CLA queue semantics

The CLA advertises N queues via `queue_count()`. These are physical or
logical transport channels — QUIC streams, DSCP classes, separate TCP
connections, etc. The CLA implements strict priority across its own
queues (queue 0 before queue 1), but does not need to understand traffic
classes. The BPA's HTB scheduler has already made the priority decision.

This means a CLA with just 2 queues (e.g., high/low QUIC streams) can
support arbitrarily many traffic classes — the HTB scheduler in the BPA
does the M→2 mapping, and the CLA just sends on whichever stream it's
told.

## Advanced Policy Patterns

The `EgressController` is the extension point for sophisticated
scheduling. Possible implementations:

- **Token Bucket** — per-class rate limiting with burst allowance
- **Weighted Fair Queueing** — proportional service across classes
- **Hierarchical Token Bucket** — multi-level rate limits with borrowing
  between parent/child classes
- **Strict Priority** — always service highest non-empty class first,
  with optional starvation guards

## Forwarding Failure Handling

When CLA transmission fails (either `NoNeighbour` result or error), `reset_peer_queue()` is called. The operation:

1. Transitions all `ForwardPending { peer, _ }` bundles to `Waiting`
2. Bundles re-enter routing via `poll_waiting()`
3. May route to different peer with fresh classification

See [Routing Design: Route Change Handling](routing_subsystem_design.md#route-change-handling).

## Data Flow Summary

```
1. INGRESS FILTER (optional)
   Set bundle.metadata.writable.flow_label based on bundle properties

2. ROUTING (process_bundle)
   RIB::find() uses flow_label for ECMP peer selection
   Returns FindResult::Forward(peer_id)

3. QUEUE CLASSIFICATION (Peer.forward)
   policy.classify(flow_label) → queue index
   Send to queue channel (fast or slow path)
   Status: ForwardPending { peer, queue }

4. QUEUE POLLING
   Poller receives bundle from channel
   Calls controller.forward(queue, bundle)

5. POLICY ENFORCEMENT (EgressController)
   Apply rate limiting, scheduling, etc.
   Calls egress_queue.forward(bundle)

6. TRANSMISSION (Dispatcher.forward_bundle)
   Load data, update extension blocks
   Run Egress filters
   CLA.forward(queue, cla_addr, data)

7. COMPLETION
   Success: delete bundle
   Failure: reset_peer_queue() → bundles return to Waiting
```

## Future direction: EgressPolicy as controller factory

The `EgressPolicy` trait currently serves two roles: classification
(`classify`) and controller creation (`new_controller`). In the target
architecture (see queue_architecture.md), these separate:

- **Classification** becomes a generic `ClassificationPolicy` trait
  (`flow_label → u32`) used at multiple pipeline stages, not just
  egress. The `EgressPolicy::classify` method is an instance of this
  generic pattern
- **Controller creation** is the factory role. `EgressPolicy` becomes
  `EgressControllerFactory` — its sole responsibility is creating
  per-peer `EgressController` instances

**`EgressPolicy::queue_count()`** currently returns M (the number of
traffic classes / BPA-side queues). This exists because the current
code in `peers.rs` pre-creates M storage channels before calling
`new_controller`. In the target architecture, the controller owns its
own queue creation — it receives N CLA queues, allocates M class queues
internally from the `QueueFactory`, and builds the M→N mapping. M
becomes a private implementation detail of the controller, not part of
the factory's public interface.

There are two separate `queue_count` methods that should not be
confused:

- **`Cla::queue_count()`** — N, the CLA's transport channel count.
  Stays on the CLA trait
- **`EgressPolicy::queue_count()`** — M, the policy's traffic class
  count. Disappears when the controller owns its queue creation

This refactor cannot be done independently of the queue architecture
work described in queue_architecture.md.
