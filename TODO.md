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
  - Location: `bpa/src/services/mod.rs`
  - Low-level trait with `Bytes` + parsed `Bundle` view
  - Key differences from Application: receives raw bundle bytes, not extracted payload

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

- [ ] **1.7 Status Report Delivery for Disconnected Applications**

  **Problem:** When a status report arrives at the BPA admin endpoint, the BPA looks up the originating service by `bundle_id.source`. For Applications (vs low-level Services), the Report-To is the BPA admin endpoint, not the application itself. If the application isn't currently registered, the status report is silently dropped.

  **Design:** Add `WaitingForService { source: Eid }` status variant:

  ```rust
  // In bpa/src/metadata.rs
  pub enum BundleStatus {
      // ... existing variants ...
      WaitingForService { source: Eid },  // Waiting for specific service to register
  }
  ```

  **Flow:**
  1. Status report arrives at admin endpoint → `administrative_bundle()`
  2. Service lookup by `report.bundle_id.source` fails (app not registered)
  3. Set `bundle.metadata.status = WaitingForService { source: report.bundle_id.source }`
  4. Persist and call `watch_bundle()` - bundle survives restarts
  5. When service registers, `ServiceRegistry::register()` calls `poll_service_waiting(&eid)`
  6. `poll_service_waiting` queries bundles with matching `WaitingForService` status
  7. Matching bundles re-processed via `ingest_bundle()` → status report delivered

  **Reaper integration:**
  - `WaitingForService` bundles are monitored by the reaper like any other bundle
  - When bundle lifetime expires, reaper drops it via `drop_bundle(bundle, ReasonCode::LifetimeExpired)`
  - Stale status reports for applications that never reconnect are automatically cleaned up
  - No special handling needed - existing reaper logic applies

  **SQLite storage impact (minimal):**
  - `from_status()`: Add case for status_code=5, source EID in `status_param3`
  - New query: `get_waiting_for_service(eid)` - `WHERE status_code = 5 AND status_param3 = ?`
  - No schema migration needed - existing columns sufficient

  **Changes required:**
  - `bpa/src/metadata.rs`: Add `WaitingForService { source: Eid }` variant
  - `bpa/src/dispatcher/admin.rs`: Use new status when service not found
  - `bpa/src/services/registry.rs`: Call `poll_service_waiting()` on registration
  - `bpa/src/dispatcher/mod.rs`: Add `poll_service_waiting()` method
  - `bpa/src/storage/mod.rs`: Add `get_waiting_for_service()` trait method
  - `sqlite-storage/src/storage.rs`: Implement status_code=5 and query
  - `bpa/src/dispatcher/restart.rs`: Handle `WaitingForService` in recovery

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
- **Implemented**: IPN Legacy filter (`ipn-legacy-filter/`) - Egress filter for 2-element IPN format compatibility
- **Implemented**: RFC9171 validity filter (`bpa/src/filters/rfc9171.rs`) - Configurable RFC9171 compliance checks (CRC, BundleAge)
- **Implemented**: Ingress metadata tracking (CLA/peer info on bundles) in `ReadOnlyMetadata`

See `bpa/docs/filter_subsystem_design.md` for design details.

### Tasks

- [x] **2.1 Add ingress metadata to bundle reception path**
  - Ingress CLA/peer info tracked in `ReadOnlyMetadata` struct
  - Available to filters at all hook points

- [x] **2.2 Implement FilterRegistry**
  - Location: `bpa/src/filters/registry.rs`
  - DAG-based ordering with `after` parameter, circular dependency detection
  - `FilterNode` linked list, `PreparedFilters` for lock-free async execution

- [x] **2.3 Integrate filters into dispatcher**
  - All four hooks: Ingress, Originate, Deliver, Egress
  - See `bpa/docs/filter_subsystem_design.md` for mutation handling per hook

---

## 3. Echo Service Implementation

Implement echo as a `Service` (low-level) that reflects bundles back to sender.

**Note:** Unlike IP where echo is ICMP (control-plane), BP echo is a **non-administrative service**. This aligns with BP's data-plane service model and will require an IETF RFC to standardize behavior.

- [ ] **3.1 Draft/track IETF RFC for BP Echo Service**
  - Define well-known service endpoint (e.g., `dtn://node/echo` or service demux)
  - Payload is opaque - echo service reflects bundles without interpreting content
  - Coordinate with DTN working group

- [x] **3.2 Implement echo service**
  - Implementation complete in `echo-service/` crate
  - Implements `Service` trait, swaps source ↔ destination, reflects bundle
  - no_std compatible using `hardy_async::sync::spin::Once`

- [x] **3.3 Register echo service during BPA initialization**
  - Feature-gated with `echo` feature (default enabled)
  - Flexible config: number, string, array of mixed, or "off" to disable
  - Default: IPN service 7 + DTN service "echo"
  - Updatable when IETF assigns official service number

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
  - Ingress metadata tracked via `ReadOnlyMetadata` (CLA name, peer node ID, peer address)
  - See Section 2.1

- [x] **7.1 Complete filter registry implementation**
  - See Section 2.2 - FilterRegistry fully implemented

- [ ] **7.2 Explicit next-hop send API**
  - `send_via(bundle, next_hop: Peer)` - bypass RIB lookup
  - Needed for probe forwarding before route exists

- [ ] **7.3 Ad-hoc routing protocol design**
  - AODV-like RREQ/RREP mechanism
  - Or echo-based probing (requires ubiquitous echo service)
  - Route TTL and maintenance

---

## 8. Integration & Testing

- [x] **8.1 Update ping tool for new echo service**
  - Reworked `tools/src/ping/` with proper status report handling
  - New payload format with sequence number, timestamps, padding for RTT measurement
  - Man page at `tools/docs/bp-ping.1.md`
  - BIB-HMAC-SHA256 signing with session key for payload integrity

- [ ] **8.2 Add integration tests**
  - [x] dtn7-rs interop tests (`tests/interop/dtn7-rs/`) - ping/echo between hardy and dtn7-rs
  - [x] Single-hop ping/echo (`tests/interop/hardy/`) - hardy-to-hardy ping
  - [ ] Neighbour resolution (BP-ARP: CL address → EID → 1-hop route)
  - [ ] SAND topology discovery and reachability states
  - [ ] Multi-hop routing with static routes
  - [ ] Dynamic peer changes (CLA add/remove neighbour)

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
│  3.1-3.3             │    │               │               │               ┌──────────────┐
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
1.0 → 1.1 → 1.2 → 1.3 → 1.5 → 1.6 → 3.2 → 3.3 → 8.1
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

## 11. BPSec Integration

BPSec (Bundle Protocol Security, RFC 9172/9173) implementation for the BPA. The `bpv7` crate has BPSec parsing/validation. The BPA has key provider infrastructure in place but lacks concrete implementations.

### Current State

| Component | Status | Location |
|-----------|--------|----------|
| BPSec parsing/validation | **Implemented** | `bpv7/src/bpsec/` |
| Key types (JWK) | **Implemented** | `bpv7/src/bpsec/key.rs` |
| RFC 9173 contexts (AES-GCM, HMAC-SHA2) | **Implemented** | `bpv7/src/bpsec/rfc9173/` |
| `KeySource` trait | **Implemented** | `bpv7/src/bpsec/key.rs` - core key lookup interface |
| `KeyProvider` trait | **Implemented** | `bpa/src/keys/mod.rs` - bundle-context-aware provider |
| `Registry` + `CompositeKeySource` | **Implemented (WIP)** | `bpa/src/keys/registry.rs` - aggregates multiple providers |
| Dispatcher integration | **Wired** | `keys_registry` field, `key_source()` method used in `admin.rs`, `local.rs` |
| Concrete KeyProvider impls | **Missing** | No config-based or file-based providers |
| BPSec reason codes in status reports | **Missing** | Dispatcher doesn't generate them |
| Decrypt failure handling | **Partial** | TODOs at `admin.rs:24`, `local.rs:202` |

### Existing Infrastructure

The key provider architecture follows a layered design:

```
┌─────────────────────────────────────────────────────────────────┐
│  KeySource trait (bpv7/src/bpsec/key.rs)                        │
│  - fn key(&self, source: &Eid, operations: &[Operation])        │
│  - Core interface used by bpv7 BPSec operations                 │
└─────────────────────────────────────────────────────────────────┘
                              ▲
┌─────────────────────────────────────────────────────────────────┐
│  KeyProvider trait (bpa/src/keys/mod.rs)                        │
│  - fn key_source(&self, bundle, data) -> Box<dyn KeySource>     │
│  - Bundle-context-aware: can make key decisions per-bundle      │
└─────────────────────────────────────────────────────────────────┘
                              ▲
┌─────────────────────────────────────────────────────────────────┐
│  Registry (bpa/src/keys/registry.rs)                            │
│  - add_provider(name, Arc<dyn KeyProvider>)                     │
│  - remove_provider(name)                                        │
│  - key_source() returns CompositeKeySource aggregating all      │
└─────────────────────────────────────────────────────────────────┘
                              ▲
┌─────────────────────────────────────────────────────────────────┐
│  Dispatcher (bpa/src/dispatcher/mod.rs)                         │
│  - keys_registry: keys::registry::Registry                      │
│  - key_source() method called by admin.rs, local.rs             │
└─────────────────────────────────────────────────────────────────┘
```

The registry is marked `#![allow(dead_code)]` because no concrete `KeyProvider` implementations are registered yet.

### Tasks

- [x] **11.1 KeyProvider trait and BPA integration**
  - `KeySource` trait defined in `bpv7/src/bpsec/key.rs`
  - `KeyProvider` trait defined in `bpa/src/keys/mod.rs`
  - `Registry` with `CompositeKeySource` in `bpa/src/keys/registry.rs`
  - Dispatcher has `keys_registry` field and `key_source()` method
  - Integration wired in `admin.rs:18-19`, `local.rs:195-196`

- [ ] **11.2 Config-based KeyProvider**
  - Implement `KeyProvider` that loads JWKS set from configuration
  - Loaded at BPA startup via config
  - Register with `keys_registry.add_provider("config", ...)`

- [ ] **11.3 File watcher KeyProvider**
  - Implement `KeyProvider` that watches JWK/JWKS files or directory
  - File watcher for runtime key updates (hot reload)
  - Register with `keys_registry.add_provider("files", ...)`

- [x] **11.4 Layered KeyProvider API**
  - `CompositeKeySource` in `bpa/src/keys/registry.rs` aggregates all registered providers
  - First key found from any source is returned
  - Provider registration order determines priority

- [ ] **11.5 Ingest error handling (junk bundles, parse failures, lost+found)**

  Handling for bundles that can't be parsed or processed during ingest/forwarding.

  **Scope:**
  - Corrupted/unintelligible bundles and blocks
  - Reassembly failures (damaged fragments)
  - BPSec signature validation failures
  - General parse errors

  **Code locations:**
  - `dispatch.rs:14` - Don't return errors for garbage content to CLA
  - `reassemble.rs:29` - Report reception failure for identifiable fragments
  - `reassemble.rs:31` - Junk bundle wrapping
  - `local.rs:210` - Junk bundle wrapping
  - `restart.rs:240` - Junk bundle recovery

  **Design decisions:**
  1. **Status report reason codes**: Generate RFC 9172 BPSec-specific reason codes for security failures
  2. **Lost+found endpoint**: Wrap damaged/unintelligible bundles for diagnostic endpoint
  3. **CLA responsibility**: Garbage content errors stay in dispatcher, not propagated to CLA

  **Requirements:** REQ-2 (BPSec compliance), REQ-14 (reliability)

- [ ] **11.6 Delivery-time decrypt failure (NoKey)**

  Handling when bundle reaches destination but payload can't be decrypted for local service.

  **Context:** Bundle is valid, parsed, and routed correctly. At final delivery, the payload block is encrypted but we don't have the decryption key. This is distinct from ingest errors - the bundle made progress through the network.

  **Code locations:**
  - `admin.rs:24` - NoKey when delivering to admin endpoint
  - `local.rs:202` - NoKey when delivering to local service

  **Design decisions:**
  1. **Drop with status report**: Generate status report with BPSec reason code, drop bundle
  2. **Hold for key availability**: Similar to `WaitingForService` (section 1.7), hold bundle until key becomes available via KeyProvider hot-reload

  **Requirements:** REQ-2 (BPSec compliance)

- [ ] **11.7 Document BPSec key provider design**
  - Create `bpa/docs/bpsec_key_provider_design.md`
  - Document the layered architecture (`KeySource` → `KeyProvider` → `Registry` → `Dispatcher`)
  - Explain bundle-context-aware key lookup pattern
  - Document `CompositeKeySource` aggregation behavior (first match wins)
  - Include examples for implementing custom `KeyProvider`
  - Reference RFC 9172/9173 security contexts

### Related

- REQ-2: Full compliance with RFC 9172 and RFC 9173
- Appendix: `bpv7/src/bpsec/encryptor.rs:190` - Update match when adding new contexts

---

## 12. Trust Model and Access Control

Access control for remote gRPC clients. In-process components (CLAs, services, routing agents compiled into the BPA) are already inside the trust perimeter - authorization checks there provide no protection.

**Design document:** `bpa/docs/trust_model_design.md`

### Architectural Decision

**Trust boundary is at the gRPC layer, not within the BPA core.**

```
┌─────────────────────────────────────────────────────────────┐
│  bpa-server process                                         │
│  ┌───────────────────────────────────────────────────────┐  │
│  │  bpa (core) - TRUSTED ZONE                            │  │
│  │  In-process CLAs, Services, Routing - no authz needed │  │
│  └───────────────────────────────────────────────────────┘  │
│                          ▲                                   │
│  ┌───────────────────────┼───────────────────────────────┐  │
│  │  proto/src/server/    │ ◄── TRUST BOUNDARY           │  │
│  │  All security enforcement happens here                │  │
│  └───────────────────────┼───────────────────────────────┘  │
└──────────────────────────┼──────────────────────────────────┘
                           │ gRPC (untrusted)
              Remote CLA / Service / Router
```

**Implications:**
- `bpa/` crate remains security-agnostic, trusts all callers
- All validation code goes in `proto/src/server/` (after Section 13 refactoring)
- In-process components bypass security checks (same process = trusted)

### Threat Summary (Remote gRPC Only)

- **T1: Malicious Remote CLA** - Source spoofing, peer impersonation
- **T2: Malicious Remote Service** - Destination scanning, resource exhaustion
- **T3: Malicious Remote Router** - Traffic redirection (future, when routing.proto exists)

### Tasks

All implementation in `proto/src/server/` (after Section 13 refactoring):

- [ ] **12.1 mTLS authentication**
  - Enable mTLS on gRPC server (require client certificates)
  - Extract certificate CN/SAN as client identity
  - Reject connections without valid certificate
  - Same mechanism for local (`localhost`) and remote connections
  - Location: `proto/src/server/mod.rs`

- [ ] **12.2 Policy file infrastructure**
  - Define policy file format (TOML) in `bpa-server`
  - Map certificate identity to policy entry
  - Implement policy loading and hot-reload
  - Pass policy context to gRPC handlers
  - Default policy: allow-all (backwards compatible)

- [ ] **12.3 Service authorization**
  - Validate EID namespace on registration
  - Validate destination namespace on send
  - Validate bundle ownership on cancel
  - Location: `proto/src/server/service.rs`

- [ ] **12.4 CLA authorization**
  - Validate peer EID namespace on `add_peer`
  - Validate source EID namespace on `dispatch`
  - Location: `proto/src/server/cla.rs`

- [ ] **12.5 Routing agent authorization** (future)
  - Validate pattern namespace on route add
  - Validate priority bounds
  - Location: `proto/src/server/routing.rs` (when created)

- [ ] **12.6 Rate limiting**
  - Per-connection rate limiting in gRPC layer
  - Integrate with TBF/HTB policy framework
  - Backpressure signaling to clients

- [ ] **12.7 BPSec source verification policy** (optional, in bpa core)
  - Orthogonal to gRPC security - provides end-to-end source auth
  - Add source verification to ingress path
  - Policy options: require, prefer, ignore signatures
  - Location: `bpa/src/dispatcher/` (exception to "no authz in bpa" - this is bundle-level)

### Requirements

- REQ-14: Reliability (security aspect)
- REQ-2: BPSec compliance (for 12.8)

---

## Recent Completions

For reference when closing external issues:

| Date | Tasks | Summary |
|------|-------|---------|
| 2026-02-20 | 2.x | RFC9171 validity filter - configurable CRC and BundleAge checks moved from parser to ingress filter for interop flexibility |
| 2026-02-20 | - | TCPCLv4 logging improvements - structured debug! syntax with address context, reduced noise (warn→debug for version mismatch, etc.) |
| 2026-02-19 | 8.2 | Hardy-to-hardy interop test, `BpaRegistration` trait with `RemoteBpa` client, gRPC restructure to `proto/src/server/`, `hardy_async::TaskPool`, component refactoring, dependency cleanup |
| 2026-02-18 | 8.2 | dtn7-rs interop tests (`tests/interop/dtn7-rs/`), centralized logging filters |
| 2026-02-17 | 8.1 | Reworked `bp ping` tool with proper status report handling, new payload format, man page |
| 2026-02-17 | - | Fix clean shutdown (storage channel, dispatcher), RIB route priority fix (Drop vs Via) |
| 2026-02-17 | - | TCPCLv4 codec fix (data extraction skipping header bytes) |
| 2026-02-10 | 3.2, 3.3 | Echo service (`echo-service/`), hardy-async `Once<T>`, OnceLock migrations |
| 2026-02-09 | 2.1, 7.0 | Ingress metadata in `ReadOnlyMetadata`, available to filters |
| 2026-02-06 | - | IPN Legacy Filter, config/serde improvements |
| 2026-02-06 | 2.3 | Egress filter, crash safety model finalized |
| 2026-02-05 | 2.3 | Filter integration, dispatcher refactoring, `processing_pool` |

---

## Appendix: Scattered Code TODOs

This section captures TODO comments found throughout the codebase that aren't yet tracked in the main sections above. Last updated: 2026-02-19.

### Functional TODOs (Production Code)

These are TODO comments in production code representing features or fixes that need implementation.

#### Dispatcher & Bundle Processing

| Topic | Locations | Description | Req |
|-------|-----------|-------------|-----|
| Ingest error handling | `dispatch.rs:14`, `reassemble.rs:29,31`, `local.rs:210`, `restart.rs:240` | **See Section 11.5** - junk bundles, parse failures, lost+found | REQ-2, REQ-14 |
| Delivery decrypt failure | `admin.rs:24`, `local.rs:202` | **See Section 11.6** - NoKey at final delivery | REQ-2 |
| Status report delivery | `admin.rs:43` | **See Section 1.7** - WaitingForService for disconnected apps | REQ-1 |
| Custody transfer | `dispatch.rs:134` | Custody transfer signalling may need to happen here | REQ-4 |
| Access control | `local.rs:155` | **See Section 12.3** - Service authorization (cancel validation in gRPC layer) | REQ-14 |

#### RIB & Routing

| Topic | Locations | Description | Req |
|-------|-----------|-------------|-----|
| Route table switching | `find.rs:22,54` | Support multiple route tables and switching between them | REQ-6 |
| Resolver lookup | `find.rs:191`, `bpa-server/clas.rs:56` | EID→CLA address resolution for `via` routes | 6.1.4 |
| Services file | `local.rs:61,74,88` | Drive local routes from a services config file | - |

#### CLA & Peers

| Location | Description | Req |
|----------|-------------|-----|
| `bpa/src/cla/registry.rs:236` | Should ideally do a replace and return the previous | - |

#### Storage

| Location | Description | Req |
|----------|-------------|-----|
| `bpa/src/metadata.rs:58` | Add 'trace' mark that will trigger local feedback | REQ-19 |
| `bpa/src/storage/adu_reassembly.rs:71` | Capture aggregate received_at across fragments | REQ-1 |
| `bpa/src/storage/adu_reassembly.rs:197` | Lots of memory copies happening here | REQ-13 |

#### BPSec

| Location | Description | Req |
|----------|-------------|-----|
| `bpv7/src/bpsec/encryptor.rs:190` | Update match when adding new contexts (**See Section 11**) | REQ-2 |
| `bpv7/src/bundle/primary_block.rs:231` | Null Report-To EID handling | REQ-1 |

#### TCPCLv4

| Topic | Locations | Description | Req |
|-------|-----------|-------------|-----|
| mTLS support | `listen.rs:376`, `connect.rs:183`, `config.rs:45` | Client/server certificate verification and config | 3.1.7 |
| gRPC registration | `tcpclv4-server/main.rs:30` | Connect to BPA via gRPC and register CLA | REQ-18 |

#### EID Patterns

| Topic | Locations | Description | Req |
|-------|-----------|-------------|-----|
| DTN glob patterns | `dtn_pattern.rs:17,60` | Glob matching needs work - split node_name/demux globs | 6.1.1, 6.1.2 |

#### Tools

| Location | Description | Req |
|----------|-------------|-----|
| `tools/src/ping/exec.rs:76` | DNS resolution for EIDs | 19.2.3 |
| `file-cla/src/watcher.rs:106` | Could implement "Sent Items" folder instead of deleting | - |

### Test Coverage Gaps (Functionality Exists, Tests Missing)

These are placeholder test functions for functionality that **already exists** but lacks test coverage. The underlying features work; these tests would verify and document the behavior.

Note: These are distinct from Functional TODOs above - no production code changes needed, only test additions.

**See also:** `bpa/docs/fuzz_test_recovery.md` - Plan to convert BPA fuzz infrastructure into proper integration tests. This migration will address many RIB/routing, local delivery, and status report test gaps listed below.

#### eid-patterns (REQ-6: 6.1.1, 6.1.2)

- `str_tests.rs:15` - Legacy IPN format (2-element) parsing: "ipn:1.2"
- `str_tests.rs:182` - Invalid syntax: "ipn:1-1", "ipn:[10-5]", "http://*"
- `str_tests.rs:221` - DTN matching logic: Exact, Prefix, Recursive

#### bpa

**Bundle Processing (REQ-1: 1.1.33):**
- `bundle.rs:40` - Age Fallback: Verify creation time derived from Age
- `bundle.rs:46` - Expiry Calculation: Verify expiry time summation

**RIB/Routing (REQ-6: 6.1.7-6.1.10):**
- `rib/find.rs:216-246` - Exact Match, Longest Prefix, Default Route, ECMP Hashing, Recursion Loop, Reflection
- `rib/route.rs:153,159` - Action Precedence, Route Entry Sort
- `rib/mod.rs:94` - Impacted Subsets: Verify `Rib::add` detects affected sub-routes
- `rib/local.rs:174-186` - Local Ephemeral, Local Action Sort, Implicit Routes

**CLA/Peers:**
- `cla/peers.rs:194,200` - Queue Selection, Queue Fallback
- `cla/mod.rs:227` - Address Parsing: Verify ClaAddress conversion
- `cla/registry.rs:323-335` - Duplicate Registration, Peer Lifecycle, Cascading Cleanup

**Policy (REQ-4: QoS):**
- `policy/mod.rs:55,61` - Flow Classification, Queue Bounds

**Services:**
- `services/registry.rs:475,481` - Duplicate Reg, Cleanup

**Node IDs (REQ-1: 1.1.23):**
- `node_ids.rs:223-241` - Single Scheme Enforce, Invalid Types, Admin Resolution (IPN/DTN)

**Storage (REQ-7, REQ-14):**
- `storage/reaper.rs:194-212` - Cache Ordering, Cache Saturation, Cache Rejection, Wakeup Trigger
- `storage/channel.rs:426-480` - Fast Path Saturation, Congestion Signal, Hysteresis Recovery, Lazy Expiry, Close Safety, Drop-to-Storage Integrity, Hybrid Duplication, Ordering Preservation, Status Consistency, Zombie Task Leak
- `storage/bundle_mem.rs:120-132` - Eviction Policy (FIFO/Priority), Min Bundles Protection
- `storage/store.rs:196-208` - Quota Enforcement, Double Delete, Transaction Rollback

#### cbor (REQ-1: 1.1.2-1.1.12)

- `decode_tests.rs:449` - Missing requirements tests

#### bpv7

**BPSec (REQ-2: 2.2.4, 2.2.7):**
- `bpsec/rfc9173/test.rs:166,169` - Wrapped Key Unwrap, Wrapped Key Fail
  - Key wrapping **is implemented** in `bib_hmac_sha2.rs` and `bcb_aes_gcm.rs`
  - Tests needed: (1) successful unwrap with valid KEK, (2) failure on corrupted wrapped key

**Bundle Parsing (REQ-1: 1.1.x):**
- `bundle/parse.rs:1196-1217` - LLR 1.1.33 (Age block), LLR 1.1.34 (Hop Count), LLR 1.1.14 (bundle rewriting), LLR 1.1.19 (extension blocks), LLR 1.1.1 (CCSDS compliance), LLR 1.1.30 (rewriting rules), LLR 1.1.12 (incomplete CBOR), Trailing Data
- `bundle/primary_block.rs:304-310` - LLR 1.1.21 (CRC values), LLR 1.1.22 (CRC types), LLR 1.1.15 (Primary Block valid)

#### tcpclv4 (REQ-3: 3.1.x)

- `session.rs:632-647` - UT-TCP-03 (Parameter Negotiation), UT-TCP-04 (Fragment Logic), UT-TCP-05 (Reason Codes)

#### sqlite-storage (REQ-7: 7.2.x)

- `migrate.rs:146,153` - SQL-01 (Migration Logic), SQL-04 (Migration Errors)
- `storage.rs:620-642` - SQL-02 (Concurrency), SQL-03 (Persistence), SQL-05 (Corrupt Data), SQL-06 (waiting_queue Invalidation)

#### localdisk-storage (REQ-7: 7.1.x)

- `storage.rs:328-365` - LD-01 (Atomic Save), LD-02 (Recovery Logic), LD-03 (Filesystem Structure), LD-04 (mmap Feature Flag), LD-05 (Persistence)

### Design/Architecture TODOs

These are higher-level design considerations captured in code.

| Location | Description | Req |
|----------|-------------|-----|
| `bpa/benches/bundle_bench.rs:3` | Entire benchmarking suite needs statistical significance work | REQ-13 |

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
