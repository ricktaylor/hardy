# Policy Subsystem Design

This document describes the egress policy subsystem in the BPA, covering flow classification, queue management, and the integration with CLA forwarding.

## Related Documents

- **[Routing Design](routing_design.md)**: RIB lookup and peer selection (ECMP uses flow_label)
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

### EgressPolicy

The main policy interface (`src/policy/mod.rs`):

```rust
pub trait EgressPolicy: Send + Sync {
    /// Number of priority queues (0 = single best-effort queue)
    fn queue_count(&self) -> u32;

    /// Classify flow_label to queue index
    fn classify(&self, flow_label: Option<u32>) -> Option<u32>;

    /// Create controller for a peer's queue set
    async fn new_controller(
        &self,
        queues: HashMap<Option<u32>, Arc<dyn EgressQueue>>,
    ) -> Arc<dyn EgressController>;
}
```

### EgressController

Policy enforcement point created per-peer:

```rust
pub trait EgressController: Send + Sync {
    /// Forward bundle through policy logic
    async fn forward(&self, queue: Option<u32>, bundle: bundle::Bundle);
}
```

### EgressQueue

Actual transmission endpoint:

```rust
pub trait EgressQueue: Send + Sync {
    /// Transmit bundle via dispatcher
    async fn forward(&self, bundle: bundle::Bundle);
}
```

## Flow Classification

### Flow Label Source

The `flow_label` is stored in bundle metadata and can be set by Ingress filters:

```rust
pub struct WritableMetadata {
    pub flow_label: Option<u32>,
}
```

See [Filter Subsystem Design](filter_subsystem_design.md) for filter implementation details.

### Classification Process

When a bundle reaches a peer for forwarding (`src/cla/peers.rs`):

```rust
pub async fn forward(&self, bundle: bundle::Bundle) -> Result<(), bundle::Bundle> {
    // 1. Extract flow_label
    let queue = if let Some(flow_label) = bundle.metadata.writable.flow_label {
        // 2. Classify to queue index
        cla.policy.classify(Some(flow_label))
    } else {
        None  // No label → best-effort
    };

    // 3. Lookup queue channel (fallback to None if invalid)
    let queue = queues.get(&queue)
        .unwrap_or_else(|| queues.get(&None).expect("No None queue"));

    // 4. Send to queue
    queue.send(bundle).await
}
```

### ECMP Peer Selection

Flow labels also affect peer selection during routing. See [Routing Design](routing_design.md) for details.

When multiple peers can reach a destination, the RIB uses a hash including `flow_label` for deterministic peer selection:

```rust
// In src/rib/find.rs
hash_one((
    &bundle.id.source,
    &bundle.destination,
    &metadata.writable.flow_label,  // Flow affinity
)) % peers.len()
```

This ensures bundles with the same flow_label always route to the same peer, preventing out-of-order delivery.

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

For each peer, queues are created via `src/cla/egress_queue.rs`:

```rust
pub fn new_queue_set(
    cla: Arc<dyn Cla>,
    dispatcher: Arc<Dispatcher>,
    peer: u32,
    cla_addr: ClaAddress,
    queue_count: u32,
) -> HashMap<Option<u32>, Arc<dyn EgressQueue>> {
    let mut h = HashMap::new();

    // Always create best-effort queue
    h.insert(None, EgressQueue::create(shared.clone(), None));

    // Create priority queues
    for i in 0..queue_count {
        h.insert(Some(i), EgressQueue::create(shared.clone(), Some(i)));
    }
    h
}
```

### Queue Pollers

Each queue has a dedicated background task (`src/cla/peers.rs`):

```rust
fn start_queue_poller(
    poll_channel_depth: usize,
    controller: Arc<dyn EgressController>,
    store: Arc<Store>,
    tasks: &TaskPool,
    peer: u32,
    queue: Option<u32>,
) -> Sender {
    let (tx, rx) = store.channel(
        BundleStatus::ForwardPending { peer, queue },
        poll_channel_depth,
    );

    spawn!(tasks, "egress_queue_poller", async move {
        while let Ok(bundle) = rx.recv_async().await {
            controller.forward(queue, bundle).await;
        }
    });

    tx
}
```

The bundle status `ForwardPending { peer, queue }` persists the queue assignment for crash recovery. See [Bundle State Machine Design](bundle_state_machine_design.md).

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

- Direct flume channel send
- Sub-millisecond latency
- Capacity: `poll_channel_depth` configuration

### Slow Path

- Bundle stored in persistent metadata with `ForwardPending` status
- Background poller drains storage to channel
- Automatic backpressure via storage insertion rate
- Hysteresis: requires <50% channel utilization to re-open fast path

## CLA Integration

### Policy Registration

When registering a CLA with the BPA (`src/bpa.rs`):

```rust
pub async fn register_cla(
    &self,
    name: String,
    address_type: Option<ClaAddressType>,
    cla: Arc<dyn Cla>,
    policy: Option<Arc<dyn EgressPolicy>>,  // Optional policy
) -> Result<Vec<NodeId>> {
    // ...
}
```

If `policy` is `None`, the default null policy (FIFO) is used.

### CLA Queue Count

CLAs can also declare their own queue count (`src/cla/mod.rs`):

```rust
pub trait Cla: Send + Sync {
    /// Number of priority queues this CLA supports
    fn queue_count(&self) -> u32 {
        0  // Default: single best-effort queue
    }

    /// Forward bundle to peer
    async fn forward(
        &self,
        queue: Option<u32>,  // Queue index passed through
        cla_addr: &ClaAddress,
        bundle: Bytes,
    ) -> Result<ForwardBundleResult>;
}
```

The queue parameter allows CLAs to implement priority scheduling at the transport level.

### Controller Lifecycle

1. **Peer initialization**: Policy creates controller via `new_controller(queues)`
2. **Bundle forwarding**: Controller mediates all `forward(queue, bundle)` calls
3. **Peer removal**: Controller dropped, pollers exit naturally

## Default Policy (Null Policy)

The null policy (`src/policy/null_policy.rs`) provides simple FIFO behavior:

```rust
impl EgressPolicy for EgressPolicy {
    fn queue_count(&self) -> u32 { 0 }

    fn classify(&self, _flow_label: Option<u32>) -> Option<u32> {
        None  // All bundles → best-effort queue
    }

    async fn new_controller(
        &self,
        queues: HashMap<Option<u32>, Arc<dyn EgressQueue>>,
    ) -> Arc<dyn EgressController> {
        let queue = queues.get(&None).expect("No None queue").clone();
        Arc::new(EgressController { queue })
    }
}

impl EgressController for EgressController {
    async fn forward(&self, _queue: Option<u32>, bundle: bundle::Bundle) {
        self.queue.forward(bundle).await  // Pass through
    }
}
```

## Advanced Policy Patterns

Controllers can implement sophisticated scheduling:

| Pattern | Description |
|---------|-------------|
| **Token Bucket** | Rate limiting with token issuance |
| **Weighted Fair Queueing** | Proportional service across priorities |
| **Hierarchical Token Bucket** | Multi-level rate limits with borrowing |

The HTB policy (`src/policy/htb_policy.rs`) provides a partial implementation of hierarchical scheduling.

## Forwarding Failure Handling

When CLA transmission fails, bundles are re-routed:

```rust
// In src/dispatcher/forward.rs
match cla.forward(queue, cla_addr, data).await {
    Ok(ForwardBundleResult::Sent) => {
        self.drop_bundle(bundle, None).await;  // Success
    }
    Ok(ForwardBundleResult::NoNeighbour) | Err(_) => {
        // Reset all bundles for this peer
        self.store.reset_peer_queue(peer).await;
    }
}
```

The `reset_peer_queue()` operation:
1. Transitions all `ForwardPending { peer, _ }` bundles to `Waiting`
2. Bundles re-enter routing via `poll_waiting()`
3. May route to different peer with fresh classification

See [Routing Design: Route Change Handling](routing_design.md#route-change-handling).

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

## Key Files

| File | Purpose |
|------|---------|
| `src/policy/mod.rs` | Policy trait definitions |
| `src/policy/null_policy.rs` | Default FIFO policy |
| `src/policy/htb_policy.rs` | Hierarchical token bucket (partial) |
| `src/cla/peers.rs` | Flow classification and queue forwarding |
| `src/cla/egress_queue.rs` | Queue set creation |
| `src/cla/registry.rs` | CLA and policy registration |
| `src/storage/channel.rs` | Hybrid fast/slow path channels |
| `src/dispatcher/forward.rs` | CLA transmission and failure handling |
