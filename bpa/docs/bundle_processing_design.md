# BPA Crate Structure Redesign

## Current Structure

```
bpa/src/
  lib.rs
  bpa.rs                  - Bpa struct, BpaRegistration trait, startup/shutdown
  builder.rs              - BpaBuilder

  dispatcher/             - god object: all bundle processing as impl Dispatcher
    mod.rs                  - Dispatcher struct (9 refs), lifecycle
    ingress.rs              - receive + parse + store + filter + checkpoint
    dispatch.rs             - queue consumer + RIB lookup
    forward.rs              - extension blocks + egress filter + CLA send
    local.rs                - deliver + originate + raw dispatch (3 concerns mixed)
    admin.rs                - admin record handling
    report.rs               - status report generation
    reassemble.rs           - fragment reassembly
    restart.rs              - recovery

  bundle/                 - bundle types
    mod.rs, core.rs, metadata.rs, status.rs

  cla/                    - convergence layer adapters
    mod.rs, registry.rs, peers.rs, egress_queue.rs

  filter/                 - filter engine
    mod.rs, engine.rs, chain.rs, rfc9171.rs, validity.rs

  key/                    - key management
    mod.rs, store.rs, pattern.rs

  rib/                    - routing information base
    mod.rs, find.rs, route.rs, agent.rs

  storage/                - persistence
    mod.rs, store.rs, channel.rs, reaper.rs, recover.rs,
    bundle_mem.rs, metadata_mem.rs, cached.rs, adu_reassembly.rs

  services/               - application services
    mod.rs, registry.rs

  policy/                 - egress policies
    mod.rs, htb_policy.rs, tbf_policy.rs, null_policy.rs

  routes.rs               - RoutingAgent trait, StaticRoutingAgent
  node_ids.rs             - NodeIds
  cbor.rs                 - CBOR precheck
  otel_metrics.rs         - metrics
```

### Problems

1. `Dispatcher` is a god object: 9 subsystem refs, 30+ methods across 8 files
2. `local.rs` mixes three concerns: origination, delivery, and raw service dispatch
3. No place for security block processing (acceptor role)
4. Bundle processing logic is coupled to lifecycle management (task pools, queues)
5. `dispatcher/dispatch.rs` wraps the RIB which already owns routing decisions

## Proposed Structure

```
bpa/src/
  lib.rs
  bpa.rs                  - Bpa struct, BpaRegistration trait
  builder.rs              - BpaBuilder

  bundle/                 - bundle types (unchanged)
    mod.rs, core.rs, metadata.rs, status.rs

  ingress.rs              - CLA → parse → security → filter → store → route
  egress.rs               - load → ext blocks → security → filter → CLA send
  delivery.rs             - route result → reassembly → service dispatch
  origination.rs          - service → build bundle → filter → store → route
  security.rs             - BPSec role-based processing (inbound + outbound)
  reporting.rs            - status report generation + admin record handling
  recovery.rs             - restart bundle recovery

  cla/                    - convergence layer adapters (unchanged)
    mod.rs, registry.rs, peers.rs, egress_queue.rs

  filter/                 - filter engine (unchanged)
    mod.rs, engine.rs, chain.rs, rfc9171.rs, validity.rs

  key/                    - key management (unchanged)
    mod.rs, store.rs, pattern.rs

  rib/                    - routing information base (unchanged)
    mod.rs, find.rs, route.rs, agent.rs

  storage/                - persistence (unchanged)
    mod.rs, store.rs, channel.rs, reaper.rs, recover.rs, ...

  services/               - application services (unchanged)
    mod.rs, registry.rs

  policy/                 - egress policies (unchanged)
    mod.rs, ...

  routes.rs               - RoutingAgent trait (unchanged)
  node_ids.rs             - NodeIds (unchanged)
  cbor.rs                 - CBOR precheck (unchanged)
  otel_metrics.rs         - metrics (unchanged)
```

### What changed

The `dispatcher/` folder is gone. Its 8 files become 7 top-level modules, each owning one flow:

| Old (dispatcher/) | New | Why |
|---|---|---|
| `ingress.rs` | `ingress.rs` | Same name, same responsibility, but standalone functions |
| `forward.rs` | `egress.rs` | "Egress" mirrors "ingress" and matches filter hook naming |
| `local.rs` (deliver part) | `delivery.rs` | Split out of the mixed `local.rs` |
| `local.rs` (originate part) | `origination.rs` | Split out of the mixed `local.rs` |
| `report.rs` + `admin.rs` | `reporting.rs` | Reports and admin records are the same concern |
| `restart.rs` | `recovery.rs` | Clearer name |
| `reassemble.rs` | moves into `delivery.rs` | Reassembly is part of the delivery flow |
| `dispatch.rs` | deleted | The RIB already owns routing decisions. Queue management moves to `ingress.rs` or `bpa.rs` |
| `mod.rs` | deleted | No more god object |

### Naming rationale

- **ingress / egress**: matches the existing filter hook names (`Hook::Ingress`, `Hook::Egress`), matches CLA direction terminology
- **delivery**: RFC 9171 Section 5.7 "Local Bundle Delivery"
- **origination**: RFC 9171 Section 5.2 "Bundle Transmission" (from local source)
- **security**: RFC 9172 "Bundle Protocol Security"
- **reporting**: covers both generating status reports and handling incoming admin records
- **recovery**: clearer than "restart" (it recovers bundles from storage, not restart the process)

### Module responsibilities

**`ingress.rs`**: the inbound pipeline. CLA calls this when bytes arrive.

```
CLA bytes → parse → security inbound → ingress filter → store → rib.find() → delivery or egress
```

One flow, one file. No `&self` god object, just function calls with explicit deps.

**`egress.rs`**: the outbound pipeline. Called when a bundle is forwarded to a peer.

```
load data → update ext blocks → security outbound (source role) → egress filter → CLA send
```

**`delivery.rs`**: local delivery. Called when `rib.find()` returns `Deliver`.

```
reassemble (if fragment) → deliver filter → service.on_receive()
```

**`origination.rs`**: local bundle creation. Called when a service sends a bundle.

```
build bundle → originate filter → store → rib.find() → delivery or egress
```

**`security.rs`**: BPSec role processing. Called from ingress (inbound) and egress (outbound).

```rust
pub fn process_inbound(bundle, data, key_store, node_ids) -> Option<(Bundle, Vec<u8>)>
pub fn process_outbound(bundle, data, key_store, destination) -> Option<(Bundle, Vec<u8>)>
```

**`reporting.rs`**: status reports. Called from any flow that needs to generate or handle reports.

```rust
pub fn report_reception(bundle, node_ids, store) -> ...
pub fn report_forwarded(bundle, node_ids, store) -> ...
pub fn report_delivery(bundle, node_ids, store) -> ...
pub fn report_deletion(bundle, reason, node_ids, store) -> ...
pub fn handle_admin_record(bundle, data) -> ...
```

**`recovery.rs`**: restart recovery. Called once at startup.

```rust
pub async fn recover_stored_bundles(store, key_store) -> ...
```

### What coordinates?

No coordinator struct. Each module is called from one of four entry points:

1. **CLA sink** (`cla/registry.rs`): calls `ingress::receive_bundle()`
2. **Service sink** (`services/registry.rs`): calls `origination::send()`
3. **Egress queue** (`cla/egress_queue.rs`): calls `egress::forward_bundle()`
4. **Startup** (`bpa.rs`): calls `recovery::recover()`

These entry points already exist. They currently call `dispatcher.method()`. They will call the module functions directly instead, passing only the refs each function needs.

### Shared state

The `Bpa` struct holds all the `Arc`s (store, rib, key_store, filter_engine, etc.) and passes them to the entry points. This is what `Bpa` already does for CLAs, services, and routes. No change to the public API.

Task pools and queues move to `Bpa` or stay on the CLA/egress infrastructure where they belong. They are not processing concerns.

## Migration

Each step is a separate commit, the system works after each:

1. Add `security.rs` (new, standalone functions)
2. Add `reporting.rs` (extract from `dispatcher/report.rs` + `dispatcher/admin.rs`)
3. Add `recovery.rs` (extract from `dispatcher/restart.rs`)
4. Add `delivery.rs` (extract deliver path from `dispatcher/local.rs`)
5. Add `origination.rs` (extract originate path from `dispatcher/local.rs`)
6. Add `egress.rs` (extract from `dispatcher/forward.rs`)
7. Add `ingress.rs` (extract from `dispatcher/ingress.rs`, integrate security + dispatch queue)
8. Delete `dispatcher/`
