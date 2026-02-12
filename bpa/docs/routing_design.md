# Routing Design

This document describes the routing infrastructure in the Bundle Protocol Agent (BPA), including the RIB, peer management, and forwarding decisions.

## Related Documents

- **[Bundle State Machine Design](bundle_state_machine_design.md)**: Bundle status transitions and crash recovery
- **[Filter Subsystem Design](filter_subsystem_design.md)**: Filter hooks that run during routing and forwarding
- **[Policy Subsystem Design](policy_subsystem_design.md)**: Flow classification and queue management
- **[Storage Subsystem Design](storage_subsystem_design.md)**: Bundle and metadata persistence

## Overview

The BPA routing system consists of three interconnected components:

| Component | Purpose | Key Structure |
|-----------|---------|---------------|
| **RIB** | Pattern-based route storage and lookup | Priority-ordered BTreeMap of EidPatterns to Actions |
| **Local Table** | Known local endpoints (services, admin) | HashMap of Eid to local Actions |
| **Peer Table** | Reachable neighbors via CLAs | HashMap of NodeId to ClaAddress to peer_id |

There is no separate FIB. Instead, forwarding decisions are recorded in bundle metadata as `BundleStatus::ForwardPending { peer, queue }`.

## Architecture Diagram

```
                          ┌─────────────────────────────────────────────┐
                          │                    RIB                      │
                          │                                             │
┌──────────────────┐      │  ┌────────────────┐    ┌─────────────────┐  │
│  Route Sources   │      │  │  Local Table   │    │   Route Table   │  │
│                  │      │  │                │    │                 │  │
│  - static_routes │─────>│  │ Eid → Actions  │    │ Priority →      │  │
│  - control plane │      │  │                │    │   Pattern →     │  │
│  - CLA peers     │      │  │ - AdminEndpoint│    │     Actions     │  │
└──────────────────┘      │  │ - Local(svc)   │    │                 │  │
                          │  │ - Forward(peer)│    │ - Drop          │  │
                          │  └────────────────┘    │ - Reflect       │  │
                          │          │             │ - Via(Eid)      │  │
                          │          │             └─────────────────┘  │
                          │          │                     │            │
                          │          └──────────┬──────────┘            │
                          │                     │                       │
                          │                     ▼                       │
                          │              ┌─────────────┐                │
                          │              │   find()    │                │
                          │              └─────────────┘                │
                          │                     │                       │
                          └─────────────────────│───────────────────────┘
                                                │
                                                ▼
                          ┌─────────────────────────────────────────────┐
                          │                FindResult                   │
                          │                                             │
                          │ AdminEndpoint │ Deliver(svc) │ Forward(peer)│ Drop
                          └─────────────────────────────────────────────┘
                                                │
                                                ▼
                          ┌─────────────────────────────────────────────┐
                          │              Peer Table                     │
                          │                                             │
                          │    peer_id → Peer { cla, queues }           │
                          │                                             │
                          │    CLA Registry:                            │
                          │    NodeId → ClaAddress → peer_id            │
                          └─────────────────────────────────────────────┘
```

## RIB (Routing Information Base)

### Data Structures

**RibInner** (`src/rib/mod.rs`):

```rust
struct RibInner {
    locals: local::LocalInner,      // Local endpoints
    routes: RouteTable,             // Pattern-based routes
    address_types: HashMap<ClaAddressType, Arc<Cla>>,
}
```

**Route Table** - Three-level nested structure:

```rust
type RouteTable = BTreeMap<u32, BTreeMap<EidPattern, BTreeSet<Entry>>>;
//                priority -> pattern -> entries
```

Lower priority numbers are checked first. Within a priority level, patterns are matched in order.

**Route Entry** (`src/rib/route.rs`):

```rust
pub struct Entry {
    pub action: routes::Action,  // Drop, Reflect, or Via(Eid)
    pub source: String,          // Origin (e.g., "static_routes", "control")
}
```

### Route Actions

| Action | Description |
|--------|-------------|
| `Drop` | Discard bundle with optional reason code |
| `Reflect` | Return to sender (previous node or source) |
| `Via(Eid)` | Forward toward the specified EID (recursive lookup) |

When multiple entries match at the same priority, precedence is: Drop > Reflect > Via.

### Local Table

The local table provides fast O(1) lookup for known local endpoints:

```rust
pub struct LocalInner {
    pub actions: HashMap<Eid, BTreeSet<local::Action>>,
    pub finals: HashSet<EidPattern>,  // Catch-all patterns
}

pub enum Action {
    AdminEndpoint,                     // Deliver to admin endpoint
    Local(Option<Arc<Service>>),       // Deliver to registered service
    Forward(u32),                      // Forward to CLA peer (next hop)
}
```

Local entries are populated by:

- **Admin endpoint**: Node's administrative EID
- **Services**: When `register_service()` is called
- **CLA peers**: When `add_peer()` creates a direct neighbor route

## Peer Table

### Structure

**PeerTable** (`src/cla/peers.rs`):

```rust
pub struct PeerTable {
    inner: RwLock<PeerTableInner>,
}

struct PeerTableInner {
    peers: HashMap<u32, Arc<Peer>>,
    next: u32,  // Auto-incrementing peer ID
}
```

**Peer**:

```rust
pub struct Peer {
    cla: Weak<Cla>,
    inner: OnceLock<PeerInner>,
}

struct PeerInner {
    queues: HashMap<Option<u32>, Sender>,  // Queue pollers
}
```

### CLA Registry Mapping

The CLA registry maintains a three-level lookup:

```rust
// In CLA struct
peers: Mutex<HashMap<NodeId, HashMap<ClaAddress, u32>>>
//            NodeId -> ClaAddress -> peer_id
```

This allows multiple addresses per node (multi-homing) and multiple nodes per CLA.

### Peer Registration Flow

```
CLA discovers neighbor
        │
        ▼
Sink::add_peer(node_id, cla_addr)
        │
        ▼
┌───────────────────────────────┐
│  CLA Registry::add_peer()     │
│                               │
│  1. Create Peer struct        │
│  2. Allocate peer_id          │
│  3. Map: NodeId/ClaAddr → id  │
│  4. Start queue pollers       │
│  5. Add local forward route   │
└───────────────────────────────┘
        │
        ▼
RIB::add_forward(node_id, peer_id)
        │
        ▼
Local table: NodeId → Forward(peer_id)
```

## Route Lookup Algorithm

### Entry Point

`RIB::find(&bundle, &mut metadata) -> Option<FindResult>`

### Algorithm

1. **Check locals first** (fast path)
   - Direct EID match in local table
   - Returns immediately if found

2. **Search route table by priority**
   - Iterate priorities low to high
   - For each priority, match patterns against destination
   - Stop at first match

3. **Handle Via(eid) recursively**
   - Recursive lookup on the via EID
   - Detects loops via trail set
   - Collects all reachable peers

4. **ECMP selection** (if multiple peers)
   - Hash of: bundle source + destination + flow_label
   - Deterministic peer selection

### FindResult

```rust
pub enum FindResult {
    AdminEndpoint,
    Deliver(Option<Arc<Service>>),
    Forward(u32),           // peer_id
    Drop(Option<ReasonCode>),
}
```

### Recursion and Via Resolution

When a route specifies `Via(eid)`, the lookup recurses:

```
Destination: ipn:200.42
Route: ipn:200.* via dtn://tunnel1

1. Match pattern ipn:200.*
2. Action: Via(dtn://tunnel1)
3. Recursive lookup: dtn://tunnel1
4. Local table: dtn://tunnel1 → Forward(peer_id=5)
5. Result: Forward(5), next_hop=dtn://tunnel1
```

The `next_hop` is stored in bundle metadata for egress filters.

## Forwarding Path

### Bundle Status Transitions

The bundle status tracks where a bundle is in the processing pipeline. See [Bundle State Machine Design](bundle_state_machine_design.md) for complete state transition details and crash recovery semantics.

```
         ┌─────────┐
         │   New   │  ← Ingress filter runs here (ingest_bundle_inner)
         └────┬────┘
              │ checkpoint after Ingress filter
              ▼
       ┌─────────────┐
       │ Dispatching │
       └──────┬──────┘
              │ process_bundle() / RIB::find()
              │
    ┌─────────┼─────────┬──────────┐
    ▼         ▼         ▼          ▼
┌───────┐ ┌───────┐ ┌────────┐ ┌─────────────────┐
│ Drop  │ │Deliver│ │Waiting │ │ ForwardPending  │
└───────┘ └───┬───┘ └───┬────┘ │ { peer, queue } │
              │         │      └────────┬────────┘
              │         │               │
     Deliver  │  route  │               │ queue poller
     filter → │  change │               │ dequeues
     service  │         │               │
              ▼         │               ▼
                        │        ┌─────────────────────────────┐
                        └───────▶│ Egress filter + CLA.forward │
                                 └─────────────────────────────┘
```

### Queue Assignment

When `FindResult::Forward(peer_id)` is returned, the bundle enters the policy subsystem. See [Policy Subsystem Design](policy_subsystem_design.md) for full details.

1. Policy classifies bundle → queue_id (based on flow_label)
2. Bundle sent to queue channel (fast path) or storage (slow path with backpressure)
3. Status: `ForwardPending { peer, queue }`
4. Queue poller receives bundle
5. **Egress filters run** (see [Filter Subsystem Design](filter_subsystem_design.md))
6. CLA forwards to peer

### Route Change Handling

When routes change, affected bundles are re-routed. See [Bundle State Machine Design: CLA Forwarding Failures](bundle_state_machine_design.md#error-handling-and-recovery) for the `reset_peer_queue` mechanism.

```
RIB::add() or RIB::remove()
        │
        ▼
Find impacted peers (via find_peers)
        │
        ▼
Store::reset_peer_queue(peer)
        │
        ▼
ForwardPending { peer, _ } → Waiting
        │
        ▼
poll_waiting_notify.notify()
        │
        ▼
Dispatcher::poll_waiting()
        │
        ▼
Re-run process_bundle() with new routes
```

## Example: Complete Forwarding Flow

See also: [Bundle State Machine Design](bundle_state_machine_design.md) for detailed state transitions and [Filter Subsystem Design](filter_subsystem_design.md) for filter hook details.

```
1. INGRESS
   Bundle arrives via tcpclv4
   Destination: ipn:200.42
   Status: New → Dispatching (after Ingress filter checkpoint)

2. ROUTE LOOKUP (process_bundle)
   RIB::find() searches:
   - locals: no match
   - routes[priority=100]: ipn:200.* via dtn://tunnel1

3. VIA RESOLUTION
   Recursive lookup: dtn://tunnel1
   - locals: Forward(peer_id=5)

4. ECMP
   Only one peer, select peer_id=5
   Set metadata.next_hop = dtn://tunnel1

5. QUEUE ASSIGNMENT
   Policy: flow_label=None → queue=None (default)
   Send to peer 5's default queue
   Status: ForwardPending { peer: 5, queue: None }

6. QUEUE POLLER (forward_bundle)
   Dequeue bundle
   Update: Previous Node, Hop Count, Bundle Age
   Run Egress filters (BPSec, validation, etc.)

7. CLA FORWARD
   Lookup ClaAddress for peer 5
   CLA::forward(None, cla_addr, bundle_bytes)

8. COMPLETION
   Success: delete bundle, send forwarded report
   Failure: reset_peer_queue(5), bundle → Waiting
```

## Synchronization

### Lock Strategy

| Component | Lock Type | Rationale |
|-----------|-----------|-----------|
| RIB | `RwLock` | Many readers (lookups), rare writers (route changes) |
| PeerTable | `spin::RwLock` | O(1) operations, minimal contention |
| CLA Registry | `spin::Mutex` | O(1) lookups, short critical sections |

### Notification Flow

```rust
// Route change triggers re-routing
rib.add_route() or rib.remove_route()
    → reset_peer_queue() for affected peers
    → poll_waiting_notify.notify_one()

// Background task responds
loop {
    poll_waiting_notify.notified().await;
    dispatcher.poll_waiting().await;
}
```

## Key Files

| File | Purpose |
|------|---------|
| `src/rib/mod.rs` | RIB structure and management |
| `src/rib/route.rs` | Route entry and add/remove logic |
| `src/rib/find.rs` | Route lookup algorithm |
| `src/rib/local.rs` | Local endpoint routing |
| `src/cla/peers.rs` | Peer table and queue management |
| `src/cla/registry.rs` | CLA registration and peer mapping |
| `src/metadata.rs` | Bundle status and forwarding state |
