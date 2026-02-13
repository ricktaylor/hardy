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
│                         Dispatcher                                   │
│                                                                      │
│  process_bundle() → RIB::find() → FindResult::Forward(peer_id)      │
│                                                                      │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                       CLA Registry                                   │
│                                                                      │
│  forward(peer_id, bundle) → PeerTable lookup                        │
│                                                                      │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                          Peer                                        │
│                                                                      │
│  1. Extract flow_label from bundle.metadata.writable.flow_label     │
│  2. policy.classify(flow_label) → queue index                       │
│  3. Send to queue channel                                           │
│                                                                      │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                  │
│  │  Queue 0    │  │  Queue 1    │  │  Queue None │                  │
│  │  (highest)  │  │  (medium)   │  │  (best-eff) │                  │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘                  │
│         │                │                │                          │
└─────────┼────────────────┼────────────────┼─────────────────────────┘
          │                │                │
          ▼                ▼                ▼
┌─────────────────────────────────────────────────────────────────────┐
│                    Queue Pollers (per queue)                         │
│                                                                      │
│  recv_async() → controller.forward(queue, bundle)                   │
│                                                                      │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      EgressController                                │
│                                                                      │
│  Policy enforcement: rate limiting, scheduling, etc.                │
│  Calls egress_queue.forward(bundle)                                 │
│                                                                      │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                   Dispatcher.forward_bundle()                        │
│                                                                      │
│  1. Load bundle data                                                │
│  2. Update extension blocks (Hop Count, Previous Node, Bundle Age)  │
│  3. Run Egress filters (see filter_subsystem_design.md)             │
│  4. CLA.forward(queue, cla_addr, data)                              │
│                                                                      │
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

### Policy Registration

When registering a CLA with the BPA, an optional `EgressPolicy` can be provided. If `None`, the default null policy (FIFO) is used.

### CLA Queue Count

CLAs can declare their own queue count via `queue_count()` (default: 0, meaning single best-effort queue). The queue index is passed to `forward()`, allowing CLAs to implement priority scheduling at the transport level.

### Controller Lifecycle

1. **Peer initialization**: Policy creates controller via `new_controller(queues)`
2. **Bundle forwarding**: Controller mediates all `forward(queue, bundle)` calls
3. **Peer removal**: Controller dropped, pollers exit naturally

## Default Policy (Null Policy)

The null policy provides simple FIFO behavior: it declares zero priority queues, classifies all bundles to the best-effort queue (`None`), and its controller simply passes bundles through without rate limiting or scheduling.

## Advanced Policy Patterns

Controllers can implement sophisticated scheduling:

| Pattern | Description |
|---------|-------------|
| **Token Bucket** | Rate limiting with token issuance |
| **Weighted Fair Queueing** | Proportional service across priorities |
| **Hierarchical Token Bucket** | Multi-level rate limits with borrowing |

The HTB policy (`src/policy/htb_policy.rs`) provides a partial implementation of hierarchical scheduling.

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

