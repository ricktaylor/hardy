# Bundle Processing State Machine Design

This document describes the bundle processing state machine in the BPA dispatcher, which tracks bundles as they transit through the processing pipeline using `metadata::BundleStatus`.

## Related Documents

- **[Routing Design](routing_design.md)**: RIB lookup and forwarding decisions in `process_bundle()`
- **[Filter Subsystem Design](filter_subsystem_design.md)**: Filter hooks that run at various state transitions
- **[Policy Subsystem Design](policy_subsystem_design.md)**: Queue assignment in `ForwardPending` status
- **[Storage Subsystem Design](storage_subsystem_design.md)**: Bundle persistence and crash recovery mechanisms

## Overview

The dispatcher implements a state machine that governs bundle lifecycle from ingestion to final disposition. Bundle state is persisted via the metadata storage backend, enabling crash recovery and resumption of in-flight bundles.

## Bundle States

The `BundleStatus` enum (defined in `bpa/src/metadata.rs`) defines all possible states:

| State | Description |
|-------|-------------|
| `New` | Initial state when bundle is first received and stored |
| `Dispatching` | Bundle queued in dispatch queue awaiting processing |
| `ForwardPending { peer, queue }` | Bundle waiting to be forwarded via a specific CLA peer |
| `AduFragment { source, timestamp }` | Fragment awaiting reassembly with other fragments |
| `Waiting` | Bundle awaiting routing opportunity (no current route available) |

## State Transition Diagram

```
                              ┌──────────────────────────────────────────┐
                              │                                          │
                              ▼                                          │
┌─────────┐    receive_    ┌─────┐    ingest_     ┌─────────────┐       │
│ Receive │───────────────▶│ New │───────────────▶│ process_    │       │
└─────────┘   bundle()     └─────┘   bundle()     │ bundle()    │       │
                              │                    └──────┬──────┘       │
                              │                           │              │
                              │         ┌─────────────────┼──────────────┼───────────────┐
                              │         │                 │              │               │
                              │         ▼                 ▼              ▼               ▼
                              │   ┌───────────┐    ┌────────────┐  ┌──────────┐   ┌───────────┐
                              │   │   Drop    │    │  Deliver   │  │ Forward  │   │ No Route  │
                              │   │ (invalid) │    │  (local)   │  │ (remote) │   │ Available │
                              │   └─────┬─────┘    └─────┬──────┘  └────┬─────┘   └─────┬─────┘
                              │         │                │              │               │
                              │         ▼                ▼              ▼               ▼
                              │   ┌───────────┐    ┌───────────┐  ┌─────────────┐ ┌─────────┐
                              │   │ Tombstone │    │ Tombstone │  │ Dispatching │ │ Waiting │
                              │   └───────────┘    └───────────┘  └──────┬──────┘ └────┬────┘
                              │                                          │              │
                              │                                          ▼              │
                              │                                   ┌────────────────┐    │
                              │                                   │ ForwardPending │    │
                              │                                   │ { peer, queue }│    │
                              │                                   └───────┬────────┘    │
                              │                                           │             │
                              │                          ┌────────────────┴─────────────┤
                              │                          │                              │
                              │                          ▼                              │
                              │                    ┌───────────┐                        │
                              │                    │ forward_  │                        │
                              │                    │ bundle()  │                        │
                              │                    └─────┬─────┘                        │
                              │                          │                              │
                              │              ┌───────────┴───────────┐                  │
                              │              │                       │                  │
                              │              ▼                       ▼                  │
                              │        ┌───────────┐          ┌───────────┐             │
                              │        │  Success  │          │  Failure  │             │
                              │        └─────┬─────┘          └─────┬─────┘             │
                              │              │                      │                   │
                              │              ▼                      └───────────────────┘
                              │        ┌───────────┐                (reset_peer_queue)
                              │        │ Tombstone │
                              │        └───────────┘
                              │
                              │    Fragment Path:
                              │
                              ▼
                        ┌───────────────┐
                        │  reassemble() │
                        └───────┬───────┘
                                │
                                ▼
                        ┌───────────────────┐
                        │    AduFragment    │
                        │ { source, ts }    │
                        └─────────┬─────────┘
                                  │
                    ┌─────────────┴─────────────┐
                    │                           │
                    ▼                           ▼
              ┌───────────┐              ┌───────────┐
              │ Complete  │              │ Incomplete│
              │ (all frags)│             │ (waiting) │
              └─────┬─────┘              └─────┬─────┘
                    │                          │
                    ▼                          ▼
              ┌───────────┐              ┌───────────┐
              │ Re-ingest │              │  Expires  │
              │ as New    │              │ (reaper)  │
              └───────────┘              └─────┬─────┘
                                               │
                                               ▼
                                         ┌───────────┐
                                         │ Tombstone │
                                         └───────────┘
```

## Detailed State Transitions

### Phase 1: Bundle Ingestion

**Entry Point:** `receive_bundle()` (`dispatch.rs`)

1. Bundle received from CLA
2. CBOR parsing and format validation
3. Bundle data stored in bundle storage
4. Metadata inserted with status **`New`**
5. Valid bundles spawn `ingest_bundle()` in processing pool
6. Invalid bundles dropped with reason code

**Processing:** `ingest_bundle_inner()` (`dispatch.rs`)

- Lifetime validation (immediate expiry check)
- Hop count validation
- **Ingress Filter Hook** execution (may drop bundle) — see [Filter Subsystem Design](filter_subsystem_design.md)
- Checkpoint: status transitions to `Dispatching`
- Proceeds to `process_bundle()`

### Phase 2: Routing Decision

**Router:** `process_bundle()` (`dispatch.rs`)

The routing lookup determines the next state. See [Routing Design](routing_design.md) for details on the RIB lookup algorithm and peer resolution.

| Route Result | Action | State Transition |
|--------------|--------|------------------|
| Drop | Bundle invalid/rejected | `Dispatching` → Tombstone |
| Admin Endpoint | Administrative handling | `Dispatching` → Tombstone |
| Local Delivery (no fragments) | Deliver to service | `Dispatching` → Tombstone |
| Local Delivery (fragments) | Fragment reassembly | `Dispatching` → `AduFragment` |
| Forward to CLA Peer | Queue for forwarding | `Dispatching` → `ForwardPending` |
| No Route Available | Wait for route | `Dispatching` → `Waiting` |

Note: Bundle enters `process_bundle()` in `Dispatching` status after the Ingress filter checkpoint.

### Phase 3: Forwarding Pipeline

See [Routing Design](routing_design.md) for details on peer table structure and queue assignment.

**Dispatch Queue:** `dispatch_bundle()` (`dispatch.rs`)

- Bundle already in **`Dispatching`** status (from Ingress checkpoint)
- Bundle sent to dispatch queue channel

**CLA Peer Queue:** (`cla/peers.rs`)

- Status transitions to **`ForwardPending { peer, queue }`**
- Bundle enters CLA-specific priority queue (see [Routing Design: Queue Assignment](routing_design.md#queue-assignment))

**Forward Execution:** `forward_bundle()` (`forward.rs`)

1. Load bundle data from store
2. Update extension blocks (Hop Count, Previous Node, Bundle Age)
3. **Egress Filter Hook** execution — see [Filter Subsystem Design](filter_subsystem_design.md)
4. Pass to CLA for transmission

| Result | Action | State Transition |
|--------|--------|------------------|
| Success | Bundle forwarded | `ForwardPending` → Tombstone |
| Failure (No Neighbor) | Re-queue for routing | `ForwardPending` → `Waiting` |

### Phase 4: Fragment Reassembly

**Reassembly:** `reassemble()` (`reassemble.rs`)

- Status transitions to **`AduFragment { source, timestamp }`**
- Fragment collected in ADU reassembly store
- Monitored by `poll_adu_fragments()`

| Condition | Action | State Transition |
|-----------|--------|------------------|
| All fragments received | Reassemble and re-ingest | `AduFragment` → `New` (new bundle) |
| Fragments incomplete | Wait for more fragments | Remains `AduFragment` |
| Lifetime expired | Drop all fragments | `AduFragment` → Tombstone |

### Phase 5: Waiting State

**Wait Monitoring:** `poll_waiting()` (`dispatch.rs`)

- Bundles in `Waiting` state periodically re-evaluated
- When route becomes available: `Waiting` → `Dispatching`
- If lifetime expires: `Waiting` → Tombstone

## Persistence Points

Bundle state is persisted at these critical moments:

| Location | Status After | Function |
|----------|--------------|----------|
| Initial storage | `New` | `receive_bundle()` |
| After Ingress filter | `Dispatching` | `ingest_bundle_inner()` |
| Waiting state | `Waiting` | `process_bundle()` |
| CLA queue entry | `ForwardPending` | `Sender::send()` |
| Fragment accumulation | `AduFragment` | `adu_reassemble()` |
| Filter mutations | Various | `ingest_bundle_inner()` |

## Error Handling and Recovery

### Lifetime Expiration

**Monitor:** Reaper Task (`reaper.rs`)

- Maintains ordered cache of bundles by expiry time
- Triggers `drop_bundle(bundle, ReasonCode::LifetimeExpired)`
- Applies to all states except bundles being actively processed

### Hop Count Exceeded

**Check:** `ingest_bundle_inner()` (`dispatch.rs`)

- Validates hop count during ingestion
- Triggers `drop_bundle(bundle, ReasonCode::HopLimitExceeded)`

### Data Loss During Processing

**Detection:** Various locations

- `load_data()` fails (data missing from storage)
- Action: `drop_bundle(bundle, None)` (silent deletion)

### Duplicate Bundle Detection

**Point 1:** CLA receive (`dispatch.rs`)
- `store.insert_metadata()` returns false
- Duplicate discarded without further processing

**Point 2:** Restart recovery (`restart.rs`)
- Bundle already in metadata store
- Spurious copy deleted

### CLA Forwarding Failures

**Location:** `forward_bundle()` (`forward.rs`)

- `reset_peer_queue(peer)` called
- All `ForwardPending { peer, _ }` bundles transition to `Waiting`
- Bundles re-evaluated by `poll_waiting()`

### Fragment Reassembly Failures

**Location:** `reassemble()` (`reassemble.rs`)

- Parse failure of reconstituted bundle
- Fragments remain in `AduFragment` status until expiry

## Channel-Based Status Management

The dispatcher uses channels with embedded status for efficient state tracking. Each channel is configured with an expected `BundleStatus`, and bundles are automatically transitioned to that status when sent through the channel. This provides implicit persistence checkpoints without explicit status management at each call site.

See `src/storage/channel.rs` for the `ChannelShared` implementation.

**Channel Types:**

| Channel | Status | Consumer |
|---------|--------|----------|
| Dispatch Queue | `Dispatching` | `run_dispatch_queue()` |
| CLA Peer Queues | `ForwardPending { peer, queue }` | CLA peer handlers |

**Channel States:**
- **Open:** In-memory channel accepts direct sends (fast path)
- **Draining:** Channel full, draining from storage (slow path)
- **Congested:** New bundles arrived during drain
- **Closing:** Channel shutting down

## Recovery Architecture

### Dual Storage System

1. **Bundle Storage:** Binary blob data (configurable backend)
2. **Metadata Storage:** Bundle state + references (configurable backend)

### Recovery Process (`recover.rs`)

1. `start_metadata_storage_recovery()` - Backend preparation
2. `bundle_storage_recovery()` - Scan all bundle data
   - Each bundle re-parsed and validated
   - `restart_bundle()` called per bundle
3. `metadata_storage_recovery()` - Find orphaned metadata
   - Metadata exists but data missing
   - Reported as deleted (DepletedStorage)

### Restart Results

| Result | Condition | Action |
|--------|-----------|--------|
| `Valid` | Data + metadata exist and match | No action needed |
| `Orphan` | Data exists, metadata missing | Insert metadata, re-ingest |
| `Duplicate` | Extra copy of existing bundle | Delete spurious copy |
| `Junk` | Unparseable data | Delete data |

## Crash Safety Strategy

1. **Save data before metadata:** Ensures data not lost if metadata insert fails
2. **Save before delete:** Ensures old data preserved if new save fails
3. **Tombstone pattern:** Metadata tombstoned last to allow recovery
4. **Lazy expiry:** Expired bundles dropped during processing, not proactively

## Concurrency Model

### Task Pools

- `processing_pool` (BoundedTaskPool): Rate-limits bundle processing
- `tasks` (TaskPool): Dispatcher and storage maintenance tasks

### Synchronization Primitives

- Bundle cache (LRU): Mutex-protected in-memory data
- Reaper cache: Mutex-protected expiry queue (BTreeSet)
- Metadata entries: Mutex-protected storage entries
- Channels: Flume bounded channels for producer/consumer

### Async Patterns

- `spawn!()` macro for task spawning
- `await` points for I/O operations
- `select_biased!()` for multi-branch waiting
- `Notify` for wakeup signals

## Key Functions Reference

| Function | File | Role |
|----------|------|------|
| `receive_bundle()` | `dispatch.rs` | Entry point for CLA-received bundles |
| `ingest_bundle()` | `dispatch.rs` | Rate-limiting wrapper, spawns processing task |
| `ingest_bundle_inner()` | `dispatch.rs` | Lifetime/hop checks, Ingress filter, checkpoint |
| `process_bundle()` | `dispatch.rs` | Routing decision hub |
| `dispatch_bundle()` | `dispatch.rs` | Queues bundle for routing |
| `originate_bundle()` | `local.rs` | Local bundle creation, Originate filter, store |
| `run_originate_filter()` | `local.rs` | Pure in-memory Originate filter execution |
| `deliver_bundle()` | `local.rs` | Local service delivery, Deliver filter |
| `forward_bundle()` | `forward.rs` | CLA submission |
| `reassemble()` | `reassemble.rs` | Fragment collection |
| `drop_bundle()` | `mod.rs` | Final deletion + reporting |
| `delete_bundle()` | `mod.rs` | Silent deletion |
| `restart_bundle()` | `restart.rs` | Recovery processing based on status checkpoint |

## Filter Execution and Crash Safety

### Overview

The dispatcher supports filter hooks at various points in the bundle lifecycle. Filters can
inspect bundles (read-only) or mutate them (read-write). This section documents the crash
safety guarantees for filter execution.

For filter traits, registration API, and execution model, see [Filter Subsystem Design](filter_subsystem_design.md).

### Filter Hooks

| Hook | Location | Execution | Persistence |
|------|----------|-----------|-------------|
| **Ingress** | `ingest_bundle_inner()` | Async (spawned task) | Always (checkpoint to `Dispatching`) |
| **Originate** | `run_originate_filter()` | Sync (in caller context) | None (bundle stored after filter) |
| **Deliver** | `deliver_bundle()` | Sync (before delivery) | None (bundle dropped after) |
| **Egress** | `forward_bundle()` | Sync (after dequeue) | None (bundle leaving node) |

### Checkpoint Model

`BundleStatus` serves as a **checkpoint marker** for crash recovery. The status indicates
"processing up to this point is complete - on restart, resume from here."

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         CHECKPOINT MODEL                                 │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│   [Receive/Create] ──► [Status: New] ──► [Ingress Filter] ──►           │
│                         (checkpoint)                                     │
│                                                                          │
│   ──► [Status: Dispatching] ──► [process_bundle()] ──► [Next State]     │
│        (checkpoint)                                                      │
│                                                                          │
│   On restart:                                                            │
│     • Status=New        → Run Ingress filter, then route                │
│     • Status=Dispatching → Skip filters, go directly to routing         │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

### Ingress Filter Crash Safety

The Ingress filter runs in a spawned task (`ingest_bundle()` uses `spawn!` on a bounded pool).
The spawning function returns to the caller once the task **starts**, not when it completes.
This means:

1. For received bundles: `receive_bundle()` returns to CLA before Ingress filter completes
2. For originated bundles: `local_dispatch()` returns `Ok(bundle_id)` before Ingress completes

If a crash occurs during or after the Ingress filter but before the status changes, the
bundle would still be in `New` status. Without proper checkpointing, the Ingress filter
would re-run on restart, potentially applying mutations twice.

**Solution:** Transition to `Dispatching` immediately after Ingress filter completes, before calling `process_bundle()`. This checkpoint is always persisted, even if the filter made no mutations. If the filter modified bundle data, the new data is saved before the old is deleted (crash-safe ordering).

See `ingest_bundle_inner()` in `src/dispatcher/dispatch.rs` for implementation.

### Originate Filter Crash Safety

The Originate filter runs **synchronously** within `originate_bundle()`, which is called by
both `local_dispatch()` and `local_dispatch_raw()`. The filter operates on an **in-memory
bundle that has not yet been stored**. The caller is blocked waiting for the result.

This design provides clean crash semantics:

- **Crash before/during filter:** Nothing persisted, caller sees failure, can retry
- **Crash after filter but before store:** Nothing persisted, caller sees failure, can retry
- **Crash after store:** Bundle is in system, Ingress filter will run on restart

No checkpoint is needed because:
1. The bundle isn't stored until after the filter passes
2. The caller handles retry semantics
3. The Ingress filter checkpoint protects against double-filtering

The `originate_bundle()` function in `src/dispatcher/local.rs` implements this pattern:
1. Wrap bundle with initial metadata (in-memory only)
2. Run Originate filter (may modify metadata like flow_label)
3. Store bundle and metadata atomically
4. Queue for Ingress filter processing

The `local_dispatch()` wrapper handles timestamp collisions by retrying with a new timestamp, while `local_dispatch_raw()` uses a fixed bundle ID without retry.

### Deliver Filter Crash Safety

The Deliver filter runs immediately before local delivery, after which the bundle is dropped.
No persistence is needed because:

1. The bundle is about to be deleted anyway
2. If crash occurs, the bundle will be re-processed from its last checkpoint
3. Re-running the Deliver filter is acceptable (idempotent delivery assumed)

See `deliver_bundle()` in `src/dispatcher/local.rs` for implementation.

### Restart Behavior

On restart, `restart_bundle()` examines the bundle status to determine where to resume:

| Status | Recovery Action |
|--------|-----------------|
| `New` | Run `ingest_bundle()` → Ingress filter → routing |
| `Dispatching` | Skip filters, run `process_bundle()` directly |
| `ForwardPending` | Re-queue for CLA transmission |
| `Waiting` | Re-add to waiting pool for route polling |
| `AduFragment` | Re-add to fragment reassembly |

### Why No Originate Checkpoint?

Originated bundles don't need a separate checkpoint state because:

1. **Delayed persistence:** The bundle isn't stored until after the Originate filter passes
2. **Caller handles failure:** If crash before store completes, caller gets no response and can retry
3. **Single persist operation:** Filter-modified metadata is preserved in the single `store()` call
4. **Ingress checkpoint:** After store, Ingress runs in a spawned task; the `Dispatching`
   checkpoint protects against re-running Ingress on restart

The transaction boundary for originated bundles is:
- Caller gets `Ok(bundle_id)` → Bundle stored and queued for Ingress filter
- Caller gets `Err` or crash → Nothing persisted, caller can retry

## Notes

- Fragment reassembly creates a new bundle with fresh `New` status
- The reaper monitors all bundles except those in active `New` processing
- Channel status management provides automatic persistence on queue transitions
