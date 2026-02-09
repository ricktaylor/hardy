# TODO: Administrative Tools, Routing API & Neighbourhood Discovery

This document tracks the implementation path toward:

- **[REQ-19](docs/requirements.md#req-19-a-well-featured-suite-of-management-and-monitoring-tools)**: "A well-featured suite of management and monitoring tools"
- **[REQ-6](docs/requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth)**: "Time-variant Routing API to allow real-time configuration of contacts and bandwidth"

The immediate goal is to make the existing `bp ping` tool functional by implementing the missing echo service. This requires foundational work on the Service trait, Filter infrastructure, and Routing Agent API that also satisfies REQ-6.

## Background

- **Ping tool**: `tools/src/ping/` - sends PING bundles, expects PONG responses
- **Current Service trait**: `bpa/src/services/mod.rs` - defines both Application (payload-only) and Service (full bundle) traits
- **Admin endpoint**: `bpa/src/dispatcher/admin.rs` - has full bundle access, only handles status reports
- **Current routing model**: Static routes only, no dynamic/ad-hoc routing agents
- **Proto definitions**: `proto/service.proto` (consolidated Application and Service endpoint APIs)

---

## 1. Trait Restructuring: Application vs Service

Align the Rust traits with the gRPC proto definitions. Both Application and Service are **endpoint APIs** for receiving bundles at registered EIDs.

**Code organization**: All service-related code should be moved into a `bpa/src/services/` directory for tidiness (currently scattered across `service.rs`, `service_registry.rs`, `dispatcher/local.rs`, etc.).

| Trait       | Access Level   | Use Case                          | Proto Equivalent      |
|-------------|----------------|-----------------------------------|-----------------------|
| Application | Payload-only   | User applications (high-level)    | `service.proto` (Application RPC) |
| Service     | Full bundle    | System services like echo         | `service.proto` (Service RPC)     |

Note: CLA (`cla.proto`) is orthogonal - it's the **network interface API**, not an endpoint API. CLAs handle bundle transmission/reception over network links, not bundle delivery to endpoints.

- [x] **1.0 Create `bpa/src/services/` directory and reorganize**
  - Move `service.rs` → `services/mod.rs` (traits)
  - Move `service_registry.rs` → `services/registry.rs`
  - Move relevant parts of `dispatcher/local.rs` → `services/`
  - Update all imports

- [x] **1.1 Rename current `Service` trait to `Application`**
  - Location: `bpa/src/services/mod.rs` (after 1.0)
  - Update all references
  - Mirrors `proto/service.proto` Application RPC naming
  - Maintains payload-only semantics: `on_receive(source, expiry, ack_requested, payload)`

- [x] **1.2 Define new `Service` trait with full bundle access**
  - Follows same bidirectional pattern as existing Service (to be renamed Application)
  - Low-level: works with `Bytes` + parsed `Bundle` view
  - Signature concept (aligned with existing pattern):

    ```rust
    #[async_trait]
    pub trait Service: Send + Sync {
        /// Called when service is registered; receives Sink for sending
        async fn on_register(&self, endpoint: &Eid, sink: Box<dyn ServiceSink>);

        /// Called when service is unregistered
        async fn on_unregister(&self);

        /// Called when a bundle arrives
        /// - `data`: raw bundle bytes (service can parse if needed with CheckedBundle::parse())
        /// - `expiry`: calculated from bundle metadata by dispatcher
        async fn on_receive(&self, data: Bytes, expiry: time::OffsetDateTime);

        /// Called when status report received for a sent bundle
        async fn on_status_notify(
            &self,
            bundle_id: &bundle::Id,
            from: &Eid,
            kind: StatusNotify,
            reason: ReasonCode,
            timestamp: Option<time::OffsetDateTime>,
        );
    }

    #[async_trait]
    pub trait ServiceSink: Send + Sync {
        /// Unregister the service
        async fn unregister(&self);

        /// Send a bundle as raw bytes
        /// - Service uses bpv7::Builder to construct
        /// - BPA parses and validates (security boundary - can't trust service)
        async fn send(&self, data: Bytes) -> Result<bundle::Id>;

        /// Cancel a pending bundle
        async fn cancel(&self, bundle_id: &bundle::Id) -> Result<bool>;
    }
    ```

  - Key differences from Application:
    - `on_receive(data, expiry)` vs `on_receive(source, expiry, ack_requested, payload)`
    - `send(data: Bytes)` vs `send(destination, data, lifetime, options)`
    - Service receives raw bundle bytes; Application receives extracted payload
  - Rationale for `Bytes` on send:
    - `bpv7::Builder` returns `Bytes`
    - BPA must parse anyway to validate (security boundary)
  - Location: `bpa/src/services/` (after 1.0)

- [x] **1.3 Create ServiceRegistry (unified)**
  - Single registry for all endpoints - RIB only deals with `Service` trait
  - Services registered at specific endpoints (e.g., `dtn://node/echo`)

- [x] **1.4 Create Application→Service proxy adapter**
  - Wraps `Application` trait to expose as `Service` internally
  - Inbound: extracts payload from bundle for Application
  - Outbound: wraps Application payload into bundle

- [x] **1.5 Update dispatcher to handle new Service trait**
  - Add `FindResult::Service(...)` for low-level services (or extend `Deliver`)
  - Keep `FindResult::AdminEndpoint` as-is (works, not blocking)
  - Dispatcher calls `Service::on_receive(data, expiry)` for low-level services
  - Dispatcher calls `Application::on_receive()` (via proxy) for applications

- [x] **1.6 Create `service.proto` for new Service trait**
  - Consolidated `service.proto` with both Application and Service RPCs
  - Shared messages (RegisterRequest, StatusNotifyRequest, etc.)
  - Service-specific messages for raw bundle bytes (ServiceSendRequest, ServiceReceiveRequest)
  - Full proxy and gRPC server implementations in `proto/proxy/service.rs` and `bpa-server/src/grpc/service.rs`

---

## 2. Filter Infrastructure

Filters are core infrastructure enabling security filters, policy enforcement, flow-labelling, and future features like ad-hoc routing.

**Use cases:**

- Security filters (block malicious bundles)
- Policy enforcement
- Flexible flow-labelling
- Logging/metrics
- Future: ad-hoc routing (Section 7)

### Current State

- **Implemented**: `ReadFilter` and `WriteFilter` async traits in `bpa/src/filters/mod.rs`
- **Implemented**: `FilterRegistry` with DAG-based ordering, dependency checking
- **Implemented**: `FilterNode` with `prepare()/exec()` pattern for lock-free async execution
- **Implemented**: `Bpa::register_filter()` and `Bpa::unregister_filter()` public API
- **Implemented**: Filter invocation at all hook points (Ingress, Deliver, Originate, Egress)
- **Implemented**: First production filter - IPN Legacy (`bpa/src/filters/ipn_legacy.rs`)
- **Implemented**: Ingress metadata tracking (CLA/peer info on bundles) in `ReadOnlyMetadata`

See `bpa/docs/filter_subsystem_design.md` for design details.

### Tasks

- [x] **2.1 Add ingress metadata to bundle reception path**
  - Extend `Sink::dispatch()` signature to pass ingress info
  - Add fields to `BundleMetadata` (in `ReadOnlyMetadata` struct):

    ```rust
    pub ingress_cla: Option<Arc<str>>,           // CLA name
    pub ingress_peer_node: Option<NodeId>,       // Peer node ID
    pub ingress_peer_addr: Option<ClaAddress>,   // Peer CL address
    ```

  - Update `receive_bundle()` to accept and store ingress info
  - Thread through dispatcher to filter invocation points
  - **Completed:** All ingress metadata is now tracked and available to filters

- [x] **2.2 Implement FilterRegistry**
  - Registry pattern like CLA/Service registries
  - Add to `Bpa` struct
  - Support filter ordering via `after` parameter (dependency chain)
  - Detect circular dependencies (error already defined)
  - Methods: `register_filter()`, `unregister_filter()`
  - **Implementation details:**
    - `FilterNode` linked list structure for DAG ordering
    - `PreparedFilters` for lock-free async execution
    - Async traits via `#[async_trait::async_trait]`
    - ReadFilters run in parallel, WriteFilters run sequentially
    - Dependency checking on removal (`HasDependants` error)

- [x] **2.3 Integrate filters into dispatcher**
  - All four hooks implemented: Ingress, Originate, Deliver, Egress
  - **Completed:**
    - Filter invocation at all hook points
    - `bundle`, `data`, and key provider passed to filters
    - `ExecResult::Drop` handled with optional reason code and bundle deletion
    - `ExecResult::Continue` mutation handling per-hook:
      - **Ingress**: Persists mutations inline, checkpoints to `Dispatching` status
      - **Originate**: No persistence (bundle stored after filter with modified metadata)
      - **Deliver**: No persistence (bundle consumed immediately after)
      - **Egress**: No persistence (bundle leaving node, may re-run on retry)
    - Added `processing_pool` (BoundedTaskPool) for rate-limited filter/bundle processing
    - Single lock acquisition per filter execution (removed `has_filters()` optimization)
    - Ingress metadata (CLA/peer info) tracked on bundles via `ReadOnlyMetadata`

- [ ] **2.4 Add filter.proto for external filters (optional)**
  - gRPC interface for out-of-process filters
  - Bidirectional stream pattern
  - May not be needed if filters are in-process only

---

## 3. Echo Service Implementation

Implement echo as a `Service` (low-level) that reflects bundles back to sender.

**Note:** Unlike IP where echo is ICMP (control-plane), BP echo is a **non-administrative service**. This aligns with BP's data-plane service model and will require an IETF RFC to standardize behavior.

- [ ] **3.1 Draft/track IETF RFC for BP Echo Service**
  - Define echo request/response bundle format
  - Define well-known service endpoint (e.g., `dtn://node/echo` or service demux)
  - Coordinate with DTN working group

- [ ] **3.2 Define echo request/response payload format**
  - NOT an administrative record (no `is_admin_record` flag)
  - Payload structure: sequence number, timestamp, optional echo data
  - Location: `bpv7/src/` or separate `echo/` crate

- [ ] **3.3 Implement echo service**
  - Implements new `Service` trait
  - Receives bundle, swaps source ↔ destination
  - Preserves timing information for RTT calculation
  - Returns response bundle (BPA validates before sending)

- [ ] **3.4 Register echo service during BPA initialization**
  - Built-in service, always available
  - Well-known endpoint per RFC (once standardized)

### Design Rationale: Echo and Routing Separation

The echo service is intentionally "dumb" - it just reflects bundles. Routing responsibility stays entirely with the RIB:

```
Source → [route exists] → Echo Service → PONG → [route back?] → Source
                                              ↓
                                         No route → Drop
```

**Route population by layer:**

| Hops | Route Source |
|------|--------------|
| 1-hop | CLA (direct peer), ARP subsystem (if only Neighbour known), or SAND |
| 2-hop | SAND (via 2-hop neighbor tracking §3.3) |
| N-hop | Static routes, or future dynamic routing agents |

**Key points:**

- If PONG is dropped (no return route), the PING sender times out → connectivity loss detected
- This is correct BFD-style behavior: no route = no connectivity = failure detected
- Echo service needs no routing awareness; it just reflects
- SAND's HEARD/SYMMETRIC states test 1-hop adjacency; echo-based probing can test multi-hop path liveness

**ARP subsystem role (Section 4):**

- Resolves 1-hop peers when CLA only knows CL address (Neighbour), not EID
- Implemented generically in BPA core - CLAs don't need duplicate resolution logic
- ARP sends BP-layer probe bundle; CLA handles CL-specific addressing for transmission
- This is possible because CLAs already support "send to CL address" - that's how they discovered the Neighbour

---

## 4. Routing Agent API

The current RIB uses function-based `add_route`/`remove_route` APIs. To support multiple routing agents (static, ad-hoc, future protocols), refactor to a trait + registry pattern consistent with CLA and Application APIs.

**Note:** Like CLA and Application/Service APIs, the RoutingAgent API requires a bidirectional Sink trait. Routing agents need to be notified of route adds, updates, and deletes by other agents for proper coordination.

- [ ] **4.1 Define `RoutingAgent` trait**
  - Bidirectional pattern like `Cla` and `Application` traits
  - Agent receives route change notifications via callbacks
  - Agent installs/withdraws routes via provided Sink

- [ ] **4.2 Define `RoutingAgentSink` trait**
  - Route methods: `add_route`, `update_route`, `remove_route`
  - **Routes use `EidPattern`** (not just `Eid`) to support wildcard destinations
  - Passed to agent on registration
  - Routes tagged with owning agent for tracking
  - Note: Peer link properties (bandwidth, MTU) come from CLA only (source of truth)
  - Routing agents manage route timing; CLAs report link characteristics

- [ ] **4.3 Create `RoutingAgentRegistry`**
  - Register/unregister routing agents
  - Route ownership tracking (which agent installed which route)
  - Broadcasts route changes to all registered agents
  - Handle agent disconnect (withdraw all its routes, notify others)

- [ ] **4.4 Create `routing.proto` for gRPC interface**
  - Bidirectional stream like `service.proto` and `cla.proto`
  - Agent→BPA: route install/withdraw requests (using `EidPattern`)
  - BPA→Agent: route change notifications from other agents
  - Enables external routing agents like DPP, hardy-tvr

- [ ] **4.6 Add specificity scoring to `eid-patterns` crate**
  - Required for DPP route selection (higher specificity wins)
  - Algorithm defined in `docs/peering.md` Section 3.2
  - IPN: based on bit depth of allocator + node
  - DTN: based on literal character count
  - Returns `(is_exact: bool, literal_length: u32)` → combined score

- [ ] **4.5 Refactor `static_routes` in bpa-server**
  - Current location: `bpa-server/` static route configuration
  - Convert to use new `RoutingAgent` trait/registry
  - Becomes a "static routing agent" that provides configured routes

---

## 5. Neighbour Resolution (BP-ARP)

Before SAND can exchange bundles with neighbors, we need to know how to reach them. Some CLAs discover Peers directly (e.g., TCPCLv4 learns EID from session), but others only discover link-layer adjacency without BPA identity.

**Terminology:**

- **Peer**: An addressable BPA with known EID + CL address
- **Neighbour**: CL adjacency only (we can reach them, but don't know their EID)

**Why this is generic (implemented in BPA core, not per-CLA):**

- Discovery probe is a BP-layer bundle - CL-agnostic content
- CLA already supports "send bundle to CL address" (that's how it communicates with discovered Neighbours)
- ARP subsystem orchestrates: receives Neighbour notification → sends probe via CLA → learns EID from response → promotes to Peer
- CLAs don't need duplicate resolution logic; they just report Neighbours and handle CL-specific transmission
- Works for any CL that can discover adjacency (TCP, UDP, Bluetooth, etc.)

**Flow:**

```
CLA discovers link-layer adjacency (Neighbour)
    → ARP-like subsystem resolves Neighbour → Peer (learns EID)
        → SAND can now exchange bundles with known Peer
```

- [ ] **5.1 Extend CLA `add_peer` API to support Neighbours**
  - Change signature: `add_peer(cl_address, eids: &[Eid])` where slice can be empty
  - Empty slice = Neighbour (CLA doesn't know EID)
  - Non-empty slice = CLA-reported EIDs (may be partial due to CL limitations)
  - Multi-homing supported: some nodes have multiple EIDs
  - Update `cla.proto` to use repeated EID field

- [ ] **5.2 Implement BP-ARP subsystem (part of CLA subsystem)**
  - Subscribes to Neighbour notifications from CLAs
  - For each Neighbour without EID:
    - Send BP-ARP probe to discover EID
    - On response: install 1-hop route in RIB
    - On failure (retries exhausted): log, no route installed
  - **Neighbour lifecycle is CLA-managed:**
    - CLA advertises Neighbour → exists
    - CLA withdraws Neighbour → gone, remove route
    - No timer-based STALE/reprobe (EIDs are stable)
  - **Route installation notifies RoutingAgents:**
    - SAND can bootstrap 2-hop discovery
    - Other agents see new reachability
  - Configurable: probe_interval, retry_count

- [ ] **5.2.1 Add ARP policy configuration**
  - `arp = "as-needed"` - Only probe if CLA provides no EIDs (default)
  - `arp = "always"` - Always probe, verify/augment CLA-provided EIDs
  - `arp = "never"` - Trust CLA, fail if no EIDs provided (closed networks)
  - Policy is administrator decision, not CLA implementor decision

- [ ] **5.3 Define ARP probe format**
  - Destination: `ipn:0.0` (LocalNode admin endpoint) - see Considerations below
  - ARP request/response as administrative record type
  - Minimal payload: just enough to elicit EID response
  - Response EID learned from bundle source field (`ipn:<their-node>.0`)
  - Could align with SAND Credential Advertisement for compatibility

- [ ] **5.4 Update CLA trait and registry**
  - `Cla` trait: `add_peer` now takes `&[Eid]` (possibly empty)
  - Registry tracks resolution state (unresolved Neighbours vs resolved Peers)
  - Callbacks for resolution completion (Neighbour → Peer promotion)

### Design Document

**Design document**: `bpa/docs/bp_arp_design.md` (TBD) will cover the full BP-ARP protocol design, including:

- Normative specification
- Security considerations
- IANA considerations
- Open questions

**Summary:** ARP probes use `ipn:!.0` (LocalNode admin endpoint) as destination. This requires an update to RFC 9758 Section 5.4 to allow externally received LocalNode EIDs for specific administrative record types. This relaxation does not break existing semantics - LocalNode remains non-routable for general use.

---

## 6. SAND Implementation

The IETF SAND draft ([draft-ietf-dtn-bp-sand-02](https://datatracker.ietf.org/doc/html/draft-ietf-dtn-bp-sand-02)) provides secure advertisement and neighborhood discovery for BPv7.

**SAND is both a Service AND a RoutingAgent:**

- **Service**: Receives/sends SAND bundles at well-known endpoint
- **RoutingAgent**: Installs routes based on discovered topology

**SAND provides:**

- Topology discovery via Local Topology Advertisement (§5.6)
- Reachability state machine: HEARD → SYMMETRIC → LOST (§5.6.1)
- CL parameter discovery via Convergence Layer Advertisement (§5.4)
- 1-hop and 2-hop neighbor tracking (§3.2, §3.3)
- Security credential exchange (§5.2)

**SAND does NOT provide (echo service fills these gaps):**

- Explicit RTT measurement
- Application-level ping (`bp ping` tool)

- [ ] **6.1 Implement SAND as Service + RoutingAgent**
  - Implements `Service` trait for bundle handling
  - Implements `RoutingAgent` trait for route installation
  - Registered at well-known SAND endpoint (IMC group + IPN/DTN service)

- [ ] **6.2 Implement SAND Information Bases**
  - Local Node Information Base (§3.1): ULN config, timers, credentials, CL instances
  - Neighbor Information Base (§3.2): 1-hop neighbors, reachability state
  - Network Information Base (§3.3): 2-hop neighbors, peer certificates

- [ ] **6.3 Implement SAND message types**
  - Data Solicitation (type 1)
  - Credential Advertisement (type 2)
  - Underlayer Advertisement (type 8)
  - Convergence Layer Advertisement (type 3)
  - Resource Advertisement (type 4)
  - Local Topology Advertisement (type 5)
  - Router Advertisement (type 6)
  - Endpoint Advertisement (type 7)

- [ ] **6.4 Implement SAND messaging modes**
  - Group Hello: multicast on network join (§6.1)
  - Targeted Hello: unicast to specific peer (§6.2)
  - Response to Solicitation (§6.3)
  - Periodic Update (§6.4)

- [ ] **6.5 Implement SAND security**
  - BPSec BIB on payload (§4.4)
  - Certificate validation and caching
  - Identity bootstrapping (§7.4)

---

## 7. Ad-hoc Multi-hop Routing (Future Work)

> **Status: DEFERRED** - This section describes AODV-like multi-hop route discovery.
> Not required for immediate goal of echo/ping. Captured here for future reference.

With SAND providing 1-hop and 2-hop neighbor discovery, and BP-ARP handling EID resolution for direct neighbours, additional ad-hoc routing may be needed for:

- Networks with 3+ hop paths
- Return-path discovery for bundles from unknown sources
- Reactive route discovery (AODV-style)

### Key Challenges

1. **Return path verification**: Bundle from unknown source doesn't imply bidirectional route
2. **Probe mechanism**: May require echo service on all nodes, or explicit next-hop send API
3. **Filter integration**: Would use `bpa/src/filters/` to observe incoming bundles
4. **Ingress metadata required**: Previous Node block does NOT identify the immediate sender

### Previous Node Block vs Ingress Metadata

**Important distinction:**

```
Source S → ... → Node X → Next-hop A → Us
                          ↑
                Previous Node block = X (the hop before A, not A itself)
```

- Previous Node block is rewritten by each forwarding node to their own EID
- When we receive from A, the block contains X (A's previous hop)
- This gives us 2-hop topology info (we learn about A's neighbours)
- But to know "this bundle came from A", we need **ingress metadata**

**Implication:** Any ad-hoc routing that needs to verify return paths or install routes via the immediate sender requires ingress CLA/peer tracking attached as bundle metadata at receive time.

### Potential Approach (AODV-like)

```
Bundle arrives from source S, no return route exists
    → Extract Previous Node (peer P, 1-hop, known)
    → Send route probe toward S via P
    → Probe propagates hop-by-hop using Previous Node
    → S responds, path established
    → Install multi-hop route
```

### Prerequisites (if implemented later)

- [x] **7.0 Add ingress CLA/peer metadata to bundles**
  - ~~**Current state: NOT TRACKED** - `receive_bundle(data: Bytes)` discards CLA/peer info~~
  - **COMPLETED:** Ingress metadata is now tracked via `ReadOnlyMetadata`:
    - `ingress_cla: Option<Arc<str>>` - CLA name
    - `ingress_peer_node: Option<NodeId>` - Peer node ID
    - `ingress_peer_addr: Option<ClaAddress>` - Peer CL address
  - `receive_bundle()` accepts ingress info and stores in bundle metadata
  - Threaded through dispatcher to filter invocation points
  - Required because Previous Node block gives 2-hop info, not immediate sender

- [x] **7.1 Complete filter registry implementation**
  - ~~Current state: `Filter` trait defined, `register_filter()` is `todo!()`~~
  - ~~Implement `FilterRegistry` (like CLA/Service registries)~~
  - **Done**: See Section 2.2 - FilterRegistry fully implemented
  - Remaining: Integrate into bundle receive path (after 7.0 for ingress-aware filters)

- [ ] **7.2 Explicit next-hop send API**
  - `send_via(bundle, next_hop: Peer)` - bypass RIB lookup
  - Needed for probe forwarding before route exists

- [ ] **7.3 Ad-hoc routing protocol design**
  - AODV-like RREQ/RREP mechanism
  - Or echo-based probing (requires ubiquitous echo service)
  - Route TTL and maintenance

---

## 8. Integration & Testing

- [ ] **8.1 Update ping tool for new echo service**
  - Verify `tools/src/ping/` works with new Service-based echo
  - May need payload format changes

- [ ] **8.2 Add integration tests**
  - Single-hop ping/echo
  - Neighbour resolution (BP-ARP: CL address → EID → 1-hop route)
  - SAND topology discovery and reachability states
  - Multi-hop routing with static routes
  - Dynamic peer changes (CLA add/remove neighbour)

---

## Dependency Graph

```
┌───────────────────────────────────────────────────────────────────────────────────────────────────┐
│                                        PARALLEL TRACKS                                            │
├───────────────────┬───────────────┬───────────────┬───────────────┬─────────┬────────────┬────────┤
│  Track A: Service │  Track B:     │  Track C: ARP │  Track D:     │ Track E │ Track F:   │Track G │
│  + Filters        │  Routing      │  + CLA/Peer   │  SAND         │ (defer) │ Link Props │(defer) │
│                   │               │               │               │         │            │        │
│  1.0 Reorganize   │  4.1 Agent    │  5.1 CLA API  │  (A+B+C)      │  7.x    │  (B+C)     │(D+F+   │
│  1.1 Rename→App   │    trait      │    Neighbour  │      ↓        │  Ad-hoc │    ↓       │ DPP)   │
│  1.2 New Service  │  4.2 Sink     │  5.2 BP-ARP   │  6.1 SAND     │         │  9.1 Peer  │   ↓    │
│  1.3 Registry     │  4.3 Registry │  5.3 Probe    │    Service    │         │    props   │ 10.x   │
│  1.4 Proxy        │  4.4 Proto    │  5.4 CLA      │  6.2-6.5      │         │  9.2-9.3   │Proactiv│
│  1.5 Dispatcher   │  4.5 Static   │       │       │               │         │  TBF/HTB   │Schedule│
│  1.6 Proto        │       │       │       │       │               │         │  9.4-9.7   │        │
│        ↓          │       │       │       │       │               │         │      │     │        │
│  2.1 Ingress meta │       │       │       │       │               │         │      │     │        │
│  2.2 FilterReg    │       │       │       │       │               │         │      │     │        │
│  2.3 Integration  │       │       │       │       │               │         │      │     │        │
└────────┬──────────┴───────┼───────┴───────┼───────┴───────┬───────┴─────────┴──────┼─────┴────────┘
         │                  │               │               │                        │
         ▼                  │               │               │                        │
┌──────────────────────┐    │               │               │                        │
│  3. Echo Service     │    │               │               │                        ▼
│  3.1-3.4             │    │               │               │               ┌──────────────┐
└──────────────────────┘    │               │               │               │ hardy-tvr/   │
                            │               │               │               │ hardy-cgr    │
                            ▼               ▼               ▼               └──────────────┘
┌─────────────────────────────────────────────────────────────────────────┐
│  8. Integration & Testing (8.1-8.2)                                     │
└─────────────────────────────────────────────────────────────────────────┘
```

**Dependencies:**

- Track B, C can start immediately (parallel to A)
- Track D (SAND) depends on A + B + C (needs Service, RoutingAgent, and resolved Peers)
- Track E (Ad-hoc) is deferred

### Critical Path

The shortest path to a working `bp ping` tool:

```
1.0 → 1.1 → 1.2 → 1.3 → 1.5 → 1.6 → 3.2 → 3.3 → 3.4 → 8.1
```

(1.4 proxy adapter not needed for echo - it's a native Service)

### Parallel Work Streams

| Track | Focus | Can Start | Blocks |
|-------|-------|-----------|--------|
| A | Service + Filter infrastructure | Immediately | Echo (3.x), SAND (6.x) |
| B | Routing agent API | Immediately | SAND (6.x), Bandwidth (9.x) |
| C | BP-ARP neighbour resolution | Immediately | SAND (6.x) |
| D | SAND implementation | After A + B + C | - |
| E | Ad-hoc multi-hop (DEFERRED) | After D | - |
| F | Bandwidth, QoS & Link Properties | After B + C | hardy-tvr, CGR, fragmentation |
| G | Proactive Scheduling (DEFERRED) | After D + F + DPP | hardy-cgr |

**Notes:**

- Tracks A, B, C can proceed in parallel
- Track D (SAND) requires all three: Service trait, RoutingAgent API, and BP-ARP
- Track E (Ad-hoc) is future work
- Track F (Bandwidth) requires RoutingAgent API (B) and CLA/Peer infrastructure (C) for link properties
- Track G (Proactive) is future work requiring DPP contact plan data and CGR algorithms; design is under review

### External Dependencies

| Item | External Dependency | Impact |
|------|---------------------|--------|
| 3.1 | IETF RFC process | Can prototype, but endpoint/format may change |
| 5.3 | draft-ietf-dtn-bp-sand | Draft is published, can implement |

### gRPC Proto Work Items

All new/updated traits must be exposed via gRPC. Summary of proto work:

| Item | Proto File | Action | Trait/Feature |
|------|------------|--------|---------------|
| 1.6 | `service.proto` | **Done** | Consolidated `Application` + `Service` endpoint APIs |
| 2.4 | `filter.proto` | **Create** (optional) | External filters via gRPC (in-process registry done) |
| 4.4 | `routing.proto` | **Create** | `RoutingAgent` + `RoutingAgentSink` |
| 5.1 | `cla.proto` | **Update** | Change `AddPeer` to use repeated EID field (empty = Neighbour) |
| 5.4 | `cla.proto` | **Update** | Add resolution completion callbacks |
| 9.7 | `cla.proto` | **Update** | Add link properties (bandwidth_bps, mtu, contact_end) to Peer |

---

## Architectural Decisions

### Echo Service Endpoint

**Decision**: Echo is a **non-administrative service** (resolved).

- Unlike ICMP in IP, BP echo uses regular bundles (no `is_admin_record` flag)
- Requires IETF RFC to standardize endpoint and payload format
- Aligns with BP's data-plane service model

### Routing Agent Deployment

**Decision**: Both in-process and gRPC deployment supported (resolved).

- Traits implemented first; gRPC service proxies to trait implementation
- In-process: implements `RoutingAgent` trait directly, lower latency
- Separate process: uses `routing.proto` gRPC API, isolation, can restart independently
- Implementation order: trait → proto → gRPC proxy (same pattern as CLA/Application)

### SAND vs Echo for Liveness Detection

**Decision**: Complementary, not either/or (resolved).

- **SAND** provides periodic-timer-based liveness (HEARD/SYMMETRIC/LOST states)
- **Echo service** provides explicit RTT measurement for diagnostics
- For ad-hoc scenarios, SAND's built-in liveness is sufficient
- Sub-millisecond BFD-style liveness is a **non-goal**

### ARP Policy as Administrator Configuration

**Decision**: ARP behavior is a BPA configuration option, not a CLA implementation choice (resolved).

- CLA reports what it knows: `add_peer(cl_address, eids: &[Eid])` where slice may be empty
- Administrator configures policy: `as-needed` (default), `always`, or `never`
- Rationale: CLAs vary in capability (some only report one EID due to CL limitations), and multi-homing means CLA-reported EIDs may be incomplete. Trust model is a deployment decision.
- This separates concerns: CLA = reports facts, BPA config = policy, ARP subsystem = execution.

### Administrative Endpoint Handling

**Decision**: Keep current implementation; refactoring is future work (resolved).

- Current `FindResult::AdminEndpoint` special case works fine
- Administrative records are a specific payload format already handled correctly
- Refactoring to Service trait is not required for echo service or SAND
- **Future consideration**: If we want external/pluggable admin handling:
  - Pre-filter for status reports (needs registry access, security-sensitive)
  - Forward other admin record types to external Service
  - Not blocking current work

### Service Registration Model

**Decision**: Single `ServiceRegistry`, Application proxied to Service (resolved).

- Internally, only `ServiceRegistry` exists - RIB deals with `Service` trait only
- `Application` trait exposed via proxy that adapts to `Service`:
  - Inbound: proxy extracts payload from bundle, calls `Application.on_receive()`
  - Outbound: proxy wraps payload into bundle, forwards to `Service` layer
- Simplifies dispatcher and RIB - single code path for all endpoints

---

## Requirements Traceability

This TODO addresses the following requirements from [requirements.md](docs/requirements.md):

| Requirement | Description | How Satisfied |
|-------------|-------------|---------------|
| **[REQ-19](docs/requirements.md#req-19-a-well-featured-suite-of-management-and-monitoring-tools)** | Management and monitoring tools | Echo service (Section 3), ping tool (8.1) |
| **[REQ-6](docs/requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth)** | Time-variant Routing API | RoutingAgent trait (Section 4) + Bandwidth infrastructure (Section 9) |
| **[6.1.4](docs/requirements.md#312-cla-apis-parent-req-6)** | EID resolution to CLA addresses | BP-ARP (Section 5) |
| **[6.1.5](docs/requirements.md#312-cla-apis-parent-req-6)** | Routes via config file | Static routes (4.5) |
| **[6.1.6](docs/requirements.md#312-cla-apis-parent-req-6)** | Add/remove routes at runtime | RoutingAgentSink (4.2) |
| **[6.1.7](docs/requirements.md#312-cla-apis-parent-req-6)** | Discard bundles by destination | RIB Drop rules (existing) |
| **[6.1.8](docs/requirements.md#312-cla-apis-parent-req-6)** | Reflect bundle to previous node | RIB Reflect rules (existing) |
| **[6.1.9](docs/requirements.md#312-cla-apis-parent-req-6)** | Prioritise routing rules | RIB priority (existing, exposed via 4.x) |
| **[6.1.10](docs/requirements.md#312-cla-apis-parent-req-6)** | ECMP | RIB (existing) |
| **[REQ-4](docs/requirements.md#req-4-alignment-with-on-going-dtn-standardisation)** | Ongoing standardization | SAND (Section 6) |
| **[REQ-14](docs/requirements.md#req-14-reliability)** | Reliability / Security | Filter infrastructure (Section 2) |

**Notes:**

- Items marked "(existing)" are already implemented in the RIB; the RoutingAgent API (Section 4) formalizes runtime access to these capabilities.
- **REQ-6 Time-variant routing**: Fully satisfied by combining:
  - Section 4: RoutingAgent API for route installation/withdrawal
  - Section 9: Bandwidth metadata on routes (capacity, rate limits)
  - A future `hardy-tvr` package would implement `RoutingAgent` with:
    - Internal contact information base (CCSDS SABRE format)
    - Timer-driven route installation when contacts become available
    - Bandwidth-aware route metadata for capacity tracking
    - Route withdrawal when contact ends or capacity exhausted

---

## 9. Bandwidth, QoS & Link Properties (Reactive Path)

This section covers the **reactive path**: queue management and sending based on currently-active links. Link properties come from CLAs (which know the transport layer characteristics). For **proactive scheduling** (forecasting based on future contacts), see Section 10.

Time-variant routing (REQ-6) and contact-based scheduling (CCSDS SABRE, CGR) require bandwidth awareness. Link properties like MTU inform fragmentation decisions. The BPA has partial infrastructure for QoS but lacks implementation.

### Architectural Model

```
Peer (link-level)                    Route (path-level)
├── bandwidth_bps (conservative)     └── next_hop → Peer
├── mtu                                  (valid while peer reachable)
├── contact_end (optional)
└── congestion state

Capacity = bandwidth_bps × (contact_end - now)  [computed on demand]
```

**Key design points:**

- **Bandwidth on Peer**: Conservative estimate of link rate (constant during contact)
- **MTU on Peer**: Maximum bundle size for fragmentation decisions
- **Contact window on Peer**: Optional `contact_end` for scheduled contacts
- **Routes**: Valid as long as next-hop peer is reachable; no per-route expiry
- **Capacity**: Computed from peer properties, single source of truth
- **Bandwidth is for planning, not policing**: Actual transmission uses full link speed

**Planning vs Transmission:**

```
Bundle arrives → "Will it fit in remaining capacity?" (conservative estimate)
    → Yes: Queue it
    → No: Reject/defer (backpressure)

Queue drains → CLA sends at actual link speed (may exceed estimate)
    → Surplus capacity → more best-effort traffic flows
```

The conservative estimate prevents over-queuing; it doesn't limit actual throughput. Best-effort traffic benefits from any surplus bandwidth.

### Current State

| Component | Status | Location |
|-----------|--------|----------|
| Flow labels | **Implemented** | `BundleMetadata.flow_label` |
| TBF/HTB policy configs | Config structures exist, **not implemented** | `bpa/src/policy/` |
| Per-peer queues | Queue exists, **no rate limiting** | `bpa/src/cla/` |
| Peer link properties | **Missing** | Peers have no bandwidth/MTU/contact_end |
| Fragmentation | **Not implemented** | PICS Item 40 is 'N' |

### Use Cases

1. **Time-variant routing (`hardy-tvr`)**: Contact schedules with bandwidth constraints
2. **CCSDS SABRE/CGR**: Contact plans with known durations and rates
3. **Queue management**: Don't queue bundles that can't possibly be sent in time
4. **Early failure detection**: Reject early rather than waiting for timeout
5. **Congestion management**: Backpressure and queue prioritization
6. **Proactive fragmentation**: Fragment bundles exceeding next-hop MTU (future)

### Tasks

- [ ] **9.1 Add link properties to Peer**
  - Extend Peer data structure:

    ```rust
    pub struct PeerLinkInfo {
        pub bandwidth_bps: Option<u64>,   // Conservative link rate estimate
        pub mtu: Option<u32>,             // Maximum bundle size
        pub contact_end: Option<OffsetDateTime>,  // When link becomes unavailable
    }
    ```

  - CLA/routing agent reports link properties via `add_peer()` or `update_peer_link_info()`
  - Bandwidth is static for contact duration
  - `contact_end = None` means permanent/always-available link
  - `contact_end = Some(time)` means scheduled contact window
  - Routes via this peer are valid while peer is reachable
  - When `contact_end` is reached, peer is removed → routes invalidate naturally

- [ ] **9.2 Capacity-aware queue management**
  - Compute remaining capacity: `bandwidth × (contact_end - now)`
  - Check before queuing: will bundle fit in remaining capacity?
  - Consider bundle size and existing queue depth
  - Reject/defer bundles that can't fit (backpressure)

- [ ] **9.3 Implement TBF (Token Bucket Filter) policy**
  - Current: `TbfConfig { rate, burst_size }` exists but unused
  - Token bucket algorithm for rate limiting
  - Per-CLA or per-peer rate limiting (for fairness/policy, not capacity)
  - Integrate with CLA send path

- [ ] **9.4 Implement HTB (Hierarchical Token Bucket) policy**
  - Current: `HtbConfig { rate, ceil, priority, quantum }` exists but unused
  - Priority classes with bandwidth sharing
  - Uses flow labels from `BundleMetadata.flow_label`
  - Parent-child rate allocation

- [ ] **9.5 Backpressure signaling**
  - Queue depth monitoring
  - CLA→BPA congestion signals
  - RoutingAgent notification of congested peers
  - May inform route selection (avoid congested next-hops)
  - Expose via metrics (REQ-19)

- [ ] **9.6 MTU-based fragmentation (future)**
  - Check next-hop Peer MTU before sending
  - Fragment bundles exceeding MTU (implements PICS Item 40)
  - Note: Currently out of scope (PICS 40 is 'N'), but link properties enable this

- [ ] **9.7 Update `cla.proto` for link properties**
  - Add `bandwidth_bps`, `mtu`, `contact_end` to Peer messages
  - Add `update_peer_link_info()` for contact window updates

### Integration with hardy-tvr

A future `hardy-tvr` package would implement `RoutingAgent` and use this infrastructure:

```rust
// Conceptual - hardy-tvr internal structures
struct Contact {
    peer: Eid,
    start_time: OffsetDateTime,
    end_time: OffsetDateTime,
    bandwidth_bps: u64,
}

// When contact becomes active:
// 1. Update peer link properties (includes contact window)
cla_sink.update_peer_link_info(peer, PeerLinkInfo {
    bandwidth_bps: Some(contact.bandwidth_bps),
    mtu: Some(9000),
    contact_end: Some(contact.end_time),
});

// 2. Install route via the peer (no expiry needed - inherits from peer)
routing_sink.add_route(Route {
    destination: peer,
    next_hop: peer,
});

// BPA computes capacity from peer properties:
// remaining = bandwidth × (contact_end - now)
// Queues bundles that fit; rejects/defers others
// Actual transmission uses full link speed (may exceed estimate)
// Surplus capacity → best-effort traffic flows

// When contact_end reached:
// - Peer removed automatically (or by routing agent)
// - Routes via peer invalidate naturally
```

### Dependency

- Depends on: Section 4 (Routing Agent API), Section 5 (CLA/Peer infrastructure)
- Enables: DPP (DTN Peering Protocol), `hardy-tvr`, CGR, SABRE integration, future fragmentation

---

## 10. Proactive Scheduling (Future Work)

> **Status: DEFERRED** - Requires contact plan store, bundle classification, and scheduling algorithms.
> This is future work beyond the immediate TODO scope, captured here for architectural context.

### The Problem

The **reactive path** (Section 9) handles "send on the best available route now." But this can be suboptimal:

```
Scenario:
  - 10 MB bulk transfer to send
  - Contact A available NOW: 1 Mbps (would take 80 seconds)
  - Contact B available in 10 MINUTES: 10 Mbps (would take 8 seconds)

Reactive decision: Send now on Contact A (80 sec total)
Optimal decision:  Wait 10 min, send on Contact B (10 min + 8 sec = 10:08 total)
```

For latency-sensitive data, reactive is correct. For bulk data with flexible deadlines, proactive scheduling can significantly improve throughput.

### Reactive vs Proactive

| Aspect | Reactive (Section 9) | Proactive (Future) |
|--------|---------------------|-------------------|
| **Input** | Current active routes/peers | Future contact schedule |
| **Decision** | Send on best available now | Wait for better opportunity? |
| **Link info from** | CLA (transport layer) | DPP ContactAdvertisements |
| **Optimizes for** | Latency | Throughput / deadline |
| **Complexity** | Low | High (CGR algorithms) |

### Two Information Stores

```
┌─────────────────────────────────────────────────────────────────┐
│                  Contact Plan Store (Future)                     │
│  Source: DPP ContactAdvertisements                               │
│  Contains: Future contacts with bandwidth, time windows          │
│  Used by: Proactive Scheduler                                    │
│                                                                  │
│  "ipn:100.* via B: 14:00-14:30 @ 1Mbps"                         │
│  "ipn:100.* via B: 15:00-16:00 @ 10Mbps"  ← better, but later   │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                  Current Peer Store (Section 9)                  │
│  Source: CLA link property reports                               │
│  Contains: Active peers with real-time properties                │
│  Used by: Reactive path (RIB, queue, send)                       │
│                                                                  │
│  "Peer B: connected, bandwidth=1Mbps, mtu=9000"                 │
└─────────────────────────────────────────────────────────────────┘
```

### Proactive Scheduler Architecture (Conceptual)

```
Bundle arrives
    │
    ▼
┌─────────────────────────────────────────┐
│         Bundle Classification           │
│  - Latency-sensitive (send ASAP)        │
│  - Bulk/flexible (can wait)             │
│  - Deadline-aware (must arrive by X)    │
└────────────────┬────────────────────────┘
                 │
                 ▼
┌─────────────────────────────────────────┐
│         Proactive Scheduler             │
│  - Query Contact Plan Store             │
│  - Compare: send now vs wait            │
│  - Consider: bundle deadline, size      │
│  - Decision: which contact to use       │
└────────────────┬────────────────────────┘
                 │
        ┌────────┴────────┐
        ▼                 ▼
┌──────────────┐  ┌──────────────────────┐
│  Send Now    │  │  Queue for Contact X │
│  (reactive)  │  │  (wait for better)   │
└──────────────┘  └──────────────────────┘
```

### Key Components

| Component | Description | Status |
|-----------|-------------|--------|
| **Contact Plan Store** | Holds future contacts from DPP; queryable by destination, time range | Not designed |
| **Bundle Classification** | Determines latency sensitivity; could use Class of Service, flow labels, or explicit flags | Not designed |
| **Scheduling Algorithm** | CGR-style optimization; considers bundle size, deadline, contact capacity | Not designed |
| **Deferred Egress** | EgressPolicy that defers bundles until better contact | **Sketched below** |

### Deferred Egress Design (Sketch)

The proactive scheduler integrates with the reactive path via a stackable `EgressPolicy` that can defer bundle transmission until a better contact window.

**Mechanism:**

1. **Extended `ForwardPending` status** - Add optional time gate:

   ```rust
   enum BundleStatus {
       ForwardPending {
           peer: u32,
           queue: Option<u32>,
           when: Option<OffsetDateTime>,  // None = immediate, Some(t) = not before t
       },
       // ...
   }
   ```

2. **Extended `ForwardResult`** - Policy can defer:

   ```rust
   enum ForwardResult {
       Sent,
       Deferred { until: OffsetDateTime },  // Set when = Some(until)
       Failed(Error),
   }
   ```

3. **Storage query** - Naturally handles both immediate and deferred:

   ```sql
   WHERE status = 'ForwardPending'
     AND peer = ? AND queue = ?
     AND (when IS NULL OR when <= now())
   ORDER BY expiry ASC
   ```

4. **Stackable EgressPolicy** - Wraps inner policy:

   ```
   ForecastPolicy (checks contact plan, may defer)
       └── RateLimitPolicy (token bucket)
           └── QueuePolicy (priority queues)
               └── actual queues
   ```

**Flow:**

```
bundle → EgressController.forward()
              │
        ForecastPolicy checks:
        "Is a better contact coming for this peer?"
              │
       ┌──────┴──────┐
       ▼             ▼
   No better     Better contact at T
       │             │
       ▼             ▼
   inner.forward()   return Deferred { until: T }
       │             │
       ▼             ▼
   Sent/Failed   Storage sets when = Some(T)
                     │
                 Bundle invisible to poller
                 until T, then auto-promoted
```

**Key Points:**

- No separate "deferred" status - just `when` field on existing status
- Storage is single source of truth (survives restart)
- Poller naturally picks up bundles when `when` passes
- Time-based only: contact activation maps to `valid_from` time
- Minimal storage implementation changes

**Packaging Note:**

The forecast scheduler (contact plan store, decision logic) and the deferred egress `EgressPolicy` implementation will likely be co-located in the same package (e.g., `hardy-cgr` or `hardy-forecast`), as they are tightly coupled - the policy needs access to the contact plan to make deferral decisions.

### DPP's Role

DPP provides **both**:

1. **Route timing** (reactive path): Installs/removes routes when contacts become active
2. **Contact forecasts** (proactive path): Populates Contact Plan Store with future contacts

The reactive path uses (1) - routes are installed when contacts are active.
The proactive path uses (2) - future contacts inform scheduling decisions.

This is why DPP's `ContactAdvertisement` includes bandwidth/time windows even though the reactive path gets link properties from CLA - the proactive scheduler needs this information for forecasting.

### Relationship to CGR

This is essentially **Contact Graph Routing (CGR)** territory:

- CCSDS has extensive work on CGR algorithms
- ION (Interplanetary Overlay Network) implements CGR
- Academic literature on optimal bundle scheduling

A future `hardy-cgr` package could implement the proactive scheduler using established CGR algorithms.

### Prerequisites

Before implementing proactive scheduling:

- [ ] Section 4 complete (RoutingAgent API for DPP integration)
- [ ] Section 9 complete (reactive path as baseline)
- [ ] DPP implemented (source of contact plan data)
- [ ] Bundle classification mechanism designed
- [ ] CGR algorithm selection/design

---

## Recent Completions

For reference when closing external issues:

### 2026-02-09: Ingress Metadata Implementation

- **Task 2.1 completed** - Ingress metadata tracking fully implemented
  - Added `ReadOnlyMetadata` struct with ingress fields:
    - `ingress_cla: Option<Arc<str>>` - CLA that received the bundle
    - `ingress_peer_node: Option<NodeId>` - Peer node ID
    - `ingress_peer_addr: Option<ClaAddress>` - Peer CL address
  - Extended `receive_bundle()` signature to accept ingress parameters
  - Ingress data captured and stored during bundle reception
  - Available to filters at all hook points
  - Also completes prerequisite 7.0 for ad-hoc routing

### 2026-02-06: IPN Legacy Filter & Config Improvements

- **IPN Legacy Filter completed** (`bpa/src/filters/ipn_legacy.rs`)
  - Feature-gated with `ipn-legacy-filter` Cargo feature
  - Egress WriteFilter that rewrites IPN 3-element EIDs to legacy 2-element format
  - Configurable `legacy-nodes` patterns for next-hop matching
  - Integrated via `bpa-server/src/filters.rs` FilterConfig tagged enum

- **Config/Serde improvements:**
  - Case-insensitive Hook enum deserialization (custom Deserialize impl)
  - Flattened tagged enums for filters, CLAs, storage (removed `content = "config"`)
  - Cleaned up redundant `#[serde(default)]` patterns
  - Updated example_config.toml and example_config.yaml with complete examples

### 2025-02-06: Egress Filter & Crash Safety Improvements

- **Egress filter implemented** (`dispatcher/forward.rs:forward_bundle`)
  - Runs after dequeue from ForwardPending, just before CLA send
  - In-memory only (like Deliver) - no persistence
  - May re-run on retry to different peer (correct for peer-specific BPSec)

- **Filter crash safety model finalized:**
  - Ingress: Checkpoints to `Dispatching` after filter (prevents re-run on restart)
  - Originate: Filter-then-store pattern (bundle stored after filter passes)
  - Deliver/Egress: No persistence (bundle consumed/leaving immediately)
  - Removed `has_filters()` optimization - single lock acquisition per filter execution

- **Removed `LocalPending` status** - unused; local delivery uses `Waiting` queue with service registration notifications

### 2025-02-05: Filter Integration & Dispatcher Improvements

- **2.3 Integrate filters into dispatcher** - Filters now invoked at Ingress, Originate, Deliver hooks
  - `ExecResult::Drop` properly deletes bundle with reason code
  - `ExecResult::Continue` mutation flags handled per-hook (see crash safety model above)
  - `processing_pool` (BoundedTaskPool) added for rate-limited bundle processing
  - Configurable via `processing_pool_size` (default: 4 × CPU cores)

- **Dispatcher refactoring:**
  - Extracted `ingest_bundle` wrapper for consistent spawn-into-pool pattern
  - `ingest_bundle_inner` does actual work; `ingest_bundle` handles spawn
  - Commonalized Originate filter logic into `run_originate_filter()` helper
  - Filter-then-store pattern for Originate (bundle not stored until filter passes)
  - Changed 14 functions from `&Arc<Self>` to `&self` (only entry points need Arc)

---

## References

### Internal

- REQ-6: Time-variant Routing API requirement
- REQ-19: Management and monitoring tools requirement
- Ping tool: `tools/src/ping/`
- Service traits: `bpa/src/services/mod.rs` (Application and Service)
- Admin dispatcher: `bpa/src/dispatcher/admin.rs`
- RIB routing: `bpa/src/rib/find.rs`
- Endpoint proto: `proto/service.proto` (consolidated Application and Service APIs)
- **BP-ARP Design:** `bpa/docs/bp_arp_design.md` (TBD)
- **DPP (DTN Peering Protocol):** `docs/dtn_peering_protocol.md`

### External / Standards

- IETF RFC TBD: BP Echo Service (to be drafted)
- IETF RFC TBD: BP-ARP (to be drafted, updates RFC 9758 Section 5.4)
- [draft-ietf-dtn-bp-sand-02](https://datatracker.ietf.org/doc/html/draft-ietf-dtn-bp-sand-02): Bundle Protocol SAND (Neighbour Discovery)
- RFC 9171: Bundle Protocol Version 7
- RFC 9758: Updates to the 'ipn' URI Scheme (defines LocalNode, constrains external reception)
