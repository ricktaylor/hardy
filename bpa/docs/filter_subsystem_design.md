# BPA Filter Subsystem Design

## Overview

The BPA filter subsystem provides hook points at strategic locations in the bundle processing pipeline, allowing pluggable filters for security, policy enforcement, flow classification, and bundle modification.

The design draws heavily from Linux netfilter's architecture—see the "Netfilter Reference" appendix for the patterns that influenced this design.

---

## Design Summary

| Aspect | Decision |
|--------|----------|
| **Hooks** | 4 hooks: Ingress, Deliver, Originate, Egress |
| **Filter Types** | 2 async traits: `ReadFilter` (read-only), `WriteFilter` (read-write) |
| **Registration** | `Filter` enum with trait object variants, `register(hook, name, after, filter)` |
| **Ordering** | DAG-based via `after` dependencies (not numeric priorities) |
| **Parallelism** | ReadFilters: parallel within a DAG level; WriteFilters: sequential |
| **Execution** | Single DAG per hook; `prepare()/exec()` split for lock-free async |
| **Result Semantics** | `Continue` = "no objection"; `Drop` = veto (stops processing) |

**Hook naming:**
- **Ingress** / **Egress**: Network boundary (CLA)
- **Deliver** / **Originate**: Service boundary

---

## Design Rationale

### Why Four Hooks?

The four hooks map to the natural decision boundaries in bundle processing:

- **Ingress**: First opportunity to reject invalid or malicious bundles before wasting resources on routing or storage. This is where size limits, source validation, and early policy checks belong.
- **Deliver**: Policy decisions that depend on the routing outcome. For example, "allow delivery to service X but not Y" can only be evaluated after the RIB determines the bundle is destined for local delivery.
- **Originate**: Enforce policy on locally-generated bundles before they enter the system. Services may attempt to send bundles that violate policy; this hook catches them early.
- **Egress**: Final validation and modification before network transmission. This is the last chance to add security blocks, validate the final bundle state, or log outbound traffic.

A FORWARD hook (like netfilter) was considered but rejected. In IP networking, forwarded packets take a different code path than locally-destined packets. In BPA, all bundles flow through the same dispatcher regardless of destination. Multi-topology routing is better handled via metadata-driven route selection than a separate filter hook.

### Why Two Filter Traits?

Separating `ReadFilter` and `WriteFilter` enables different execution models optimised for each use case:

**ReadFilters** only inspect bundles and return a pass/fail verdict. Because they don't modify state, multiple ReadFilters can safely run in parallel. This improves throughput for common validation tasks like size checks, ACL lookups, and source validation.

**WriteFilters** may modify the bundle bytes or metadata. These modifications must be serialised—each WriteFilter needs to see the result of previous modifications. Running them in parallel would create race conditions and non-deterministic results.

This separation mirrors netfilter's distinction between the `filter` table (accept/drop decisions) and `mangle` table (packet modification). The key insight is that read-only operations can be parallelised, while mutations require ordering.

### Why DAG-Based Ordering?

Netfilter uses numeric priorities (e.g., `-300` for conntrack, `-150` for mangle, `0` for filter) to order callbacks. This approach has significant drawbacks:

- **Magic numbers**: `-300` vs `-200` conveys no semantic meaning without documentation
- **Collision risk**: Two filters wanting the same priority must coordinate
- **Fragile insertion**: Adding a filter between `-150` and `-100` requires picking `-125` and hoping nothing else uses it
- **Implicit dependencies**: "Why does X run before Y?" requires tracing priority values

DAG-based `after` dependencies address all of these:

- **Self-documenting**: `after: ["add_bib"]` explicitly states the relationship
- **No collisions**: Filter names are unique; ordering conflicts are impossible
- **Automatic parallelism**: The executor identifies filters with no mutual dependencies and runs them concurrently
- **Explicit errors**: Missing dependencies and cycles produce clear error messages at registration time

The tradeoff is slightly more complex registration (must name dependencies), but this is a one-time cost that pays off in maintainability.

### Why Async Traits?

Filters may need to perform operations that shouldn't block the executor:

- **gRPC calls** to external policy engines or centralised security services
- **Database lookups** for access control lists or reputation data
- **Cryptographic operations** that benefit from async I/O (e.g., HSM interactions)
- **Rate limiting** with async timers

Synchronous filter traits would force these operations to block, reducing throughput and potentially causing executor starvation. Async traits (via `#[async_trait]`) allow filters to yield while waiting, enabling the executor to process other bundles concurrently.

### Why prepare()/exec() Split?

The filter registry uses a sync `RwLock` to protect filter storage. This creates a problem for async execution:

1. **Send-safety**: `std::sync::RwLockReadGuard` is not `Send`. Holding it across `.await` points would make the future non-Send, incompatible with multi-threaded async runtimes like Tokio.
2. **Writer starvation**: If filters hold the read lock during execution (which may take milliseconds for gRPC calls), registration and unregistration operations would be blocked for extended periods.

The solution is a two-phase execution model:

1. **`prepare()`**: Acquire the read lock briefly, clone only the `Arc` references to the filters (cheap refcount increments), then release the lock immediately.
2. **`exec()`**: Run the prepared filters without holding any lock.

This keeps lock hold times in the microsecond range regardless of filter execution time, and produces a Send-safe future suitable for any async runtime.

### Why Continue/Drop Semantics?

The result semantics differ subtly from netfilter's ACCEPT/DROP:

| Netfilter | BPA | Meaning |
|-----------|-----|---------|
| `NF_ACCEPT` | — | "Accept the packet, stop this chain" (final positive decision) |
| `NF_DROP` | `Drop` | "Reject, stop processing" (final negative decision) |
| — | `Continue` | "I have no objection, but others still vote" (not final) |

BPA uses a "unanimous consent" model:

- **Any single `Drop`** immediately vetoes the bundle and stops processing
- **All filters must `Continue`** for the bundle to proceed

This model is appropriate for security-critical filtering. A bundle should only proceed if no filter objects. Filters don't need to coordinate or know about each other—they simply vote independently, and any veto is final.

The optional `ReasonCode` in `Drop` allows filters to indicate why the bundle was rejected, enabling status report generation with meaningful diagnostic information.

---

## Implementation Status

Core filter infrastructure is implemented in `bpa/src/filters/`:

| Component | File | Status |
|-----------|------|--------|
| Filter traits (`ReadFilter`, `WriteFilter`) | `filter.rs` | ✅ Implemented |
| Result types (`FilterResult`, `RewriteResult`) | `filter.rs` | ✅ Implemented |
| `Mutation` flags for persistence | `registry.rs` | ✅ Implemented |
| Error types | `mod.rs` | ✅ Implemented |
| `FilterNode` (DAG-based execution) | `filter.rs` | ✅ Implemented |
| `PreparedFilters` (lock-free execution) | `filter.rs` | ✅ Implemented |
| `Registry` (per-hook filter storage) | `registry.rs` | ✅ Implemented |
| `Bpa::register_filter()` | `bpa.rs` | ✅ Implemented |
| `persist_filter_mutation()` | `dispatcher/mod.rs` | ✅ Implemented |

**Hook integration status:**

| Hook | Location | Status |
|------|----------|--------|
| Ingress | `dispatcher/dispatch.rs:ingest_bundle_inner` | ✅ Implemented |
| Deliver | `dispatcher/local.rs:deliver_bundle` | ✅ Implemented |
| Originate | `dispatcher/local.rs:run_originate_filter` | ✅ Implemented |
| Egress | TBD | 🔲 Stub added, placement TBD |

**Rate limiting:**

Filter execution runs through a `BoundedTaskPool` (`processing_pool`) to prevent resource exhaustion. The pool size is configurable via `processing_pool_size` (default: 4 × CPU cores).

---

## Filter Traits

See `bpa/src/filters/filter.rs` for trait definitions and result types.

Two async traits with identical signatures, differing only in return type:

- **`ReadFilter`**: Read-only inspection, returns `FilterResult` (`Continue` or `Drop`)
- **`WriteFilter`**: May modify bundle, returns `RewriteResult` with optional new metadata/data

The `RewriteResult::Continue` variant carries optional modifications:
- `(None, None)` — no change
- `(Some(meta), None)` — metadata changed, bundle bytes unchanged
- `(None, Some(data))` — bundle bytes changed (rare)
- `(Some(meta), Some(data))` — both changed

### Mutation Tracking and Persistence

The filter chain aggregates modifications into a `Mutation` struct (see `registry.rs`):

```rust
pub struct Mutation {
    pub metadata: bool,  // true if any filter modified metadata
    pub bundle: bool,    // true if any filter modified bundle data
}
```

After `ExecResult::Continue`, the dispatcher calls `persist_filter_mutation()` to save changes:

1. **If `mutation.bundle`**: Save new bundle data to store, delete old data (crash-safe order), update `storage_name` in metadata
2. **If `mutation.metadata` or `mutation.bundle`**: Update metadata in store

This ensures filter modifications survive restarts and are visible to subsequent processing stages.

**Exception:** The Deliver hook does not persist changes because the bundle is immediately delivered (using in-memory modified data) and then dropped.

---

## Registration API

See `bpa/src/filters/mod.rs` for the `Filter` enum and `Hook` enum, and `bpa/src/filters/registry.rs` for the `Registry` and `ExecResult` types.

The `Filter` enum wraps either a `ReadFilter` or `WriteFilter` trait object in an `Arc`. The `Hook` enum identifies which hook point to register at.

### Registry Methods

- **`register(hook, name, after, filter)`** — Add a filter with explicit dependencies
- **`unregister(hook, name)`** — Remove a filter (fails if other filters depend on it)
- **`exec(hook, ...)`** — Execute all filters at a hook point

### Public API via `Bpa`

See `bpa/src/bpa.rs:register_filter()` and `unregister_filter()` for the public interface.

Filters are registered with a unique name and optional `after` dependencies. Filter names must be unique within a hook (not globally), since each hook maintains its own DAG and `after` dependencies are resolved per-hook. Unregistration checks for dependants and fails if other filters would be orphaned.

---

## Execution Model

### DAG-Based Ordering

Filters declare dependencies via `after`. The DAG executor:
1. Resolves dependencies at registration time
2. Groups filters by "level" (same dependencies satisfied)
3. Runs ReadFilters in parallel within a level
4. Runs WriteFilters sequentially
5. Stops immediately on any `Drop` result

```
Example: Egress hook

    ┌───────────┐
    │ add_meta  │  (WriteFilter, after: [])
    └─────┬─────┘
          ▼
    ┌───────────┐
    │  add_bib  │  (WriteFilter, after: ["add_meta"])
    └─────┬─────┘
          ▼
    ┌───────────┐
    │  add_bcb  │  (WriteFilter, after: ["add_bib"])
    └─────┬─────┘
          ▼
 ┌──────────┐ ┌──────────┐
 │ validate │ │ acl_chk  │  (ReadFilters, after: ["add_bcb"])
 └────┬─────┘ └────┬─────┘  ← run in parallel
      └──────┬─────┘
             ▼
         Continue
```

### Lock-Free Async Execution

The `prepare()/exec()` split avoids holding sync locks across await points:

1. **`prepare()`**: Briefly holds read lock, clones `Arc` refs only
2. **`exec()`**: Runs without any lock, safe for async

This prevents writer starvation and is Send-safe for async runtimes.

### Rate-Limited Execution

Filter execution occurs within the dispatcher's `processing_pool` (a `BoundedTaskPool`). This:

- Prevents unbounded parallelism from exhausting system resources
- Applies backpressure when the pool is saturated
- Is configurable via `processing_pool_size` (default: 4 × available CPU cores)

The pool is shared across all bundle processing work (ingress, filter execution, dispatch).

### Parallelism Rules

| Trait | Parallelism |
|-------|-------------|
| **ReadFilter** | Parallel with other ReadFilters at same DAG level |
| **WriteFilter** | Sequential (rewrites chain through each filter) |

---

## Hook Points

### Bundle Processing Flow

```
CLA.on_receive(data)
  └─▶ dispatcher.receive_bundle(data)
        ├─ parse bundle
        ├─ save to store
        └─▶ ingest_bundle(bundle)  ← spawns into processing_pool
              └─▶ ingest_bundle_inner(bundle)
                    ├─ check lifetime/hop count
                    ├─ ◀── HOOK: Ingress
                    ├─ persist_filter_mutation() if changes
                    └─▶ process_bundle(bundle)
                          ├─ RIB lookup
                          ├─ Deliver:
                          │     ├─ ◀── HOOK: Deliver
                          │     └─ deliver_bundle(service)
                          │           └─ (no persist - bundle dropped after delivery)
                          └─ Forward → egress path

Local origination:
  └─▶ local_dispatch(...)
        ├─ Builder::build() or CheckedBundle::parse()
        ├─ store.store(bundle, data)  ← store FIRST
        ├─ ◀── HOOK: Originate (via run_originate_filter)
        ├─ persist_filter_mutation() if changes
        └─▶ ingest_bundle(bundle)

Status reports:
  └─▶ dispatch_status_report(...)
        ├─ Builder::build()
        ├─ store.store(bundle, data)  ← store FIRST
        ├─ ◀── HOOK: Originate (via run_originate_filter)
        ├─ persist_filter_mutation() if changes
        └─▶ dispatch_bundle(bundle)

Egress path:
  └─▶ peers.forward(bundle)
        ├─ ◀── HOOK: Egress (TBD)
        └─ Peer queue → CLA.send()
```

**Store-first-then-filter pattern:** For Originate hooks, the bundle is stored before filtering. This ensures filters see real metadata (storage_name, status) rather than placeholder values. If the filter drops the bundle, it is deleted from the store.

### Hook Placement

| Hook | Position | Use Cases |
|------|----------|-----------|
| **Ingress** | After parse, before routing | Size limits, source validation, flow classification |
| **Deliver** | After RIB "Deliver", before service | Service access control, metadata injection |
| **Originate** | After store, before dispatch | Source policy, add security blocks (sees real metadata) |
| **Egress** | Before CLA send | Final validation, encryption, logging |

---

## Typical Filter Usage

| Hook | ReadFilter | WriteFilter |
|------|------------|-------------|
| **Ingress** | Size limits, source validation | Flow classification, storage policy |
| **Deliver** | Service access control | Add extension blocks |
| **Originate** | Source policy enforcement | Flow label, add BIB |
| **Egress** | Final validation | Add BIB, BCB |

### Well-Known Filter Names

| Name | Trait | Purpose |
|------|-------|---------|
| `size_check` | Read | Reject oversized bundles |
| `source_validator` | Read | Validate bundle source against policy |
| `destination_acl` | Read | Enforce destination access control |
| `flow_classifier` | Write | Set flow label based on bundle properties |
| `ipn_2_element` | Write | Rewrite IPN EIDs to legacy encoding |
| `add_bib` | Write | Add Bundle Integrity Block |
| `add_bcb` | Write | Add Bundle Confidentiality Block |

---

## Future Work

### Ingress Metadata

Filters may need CLA/peer information for policy decisions. The `BundleMetadata` struct (see `bpa/src/metadata.rs`) could be extended with `ingress_cla` and `ingress_peer` fields to provide this context.

### External Filters via gRPC

A `filter.proto` could enable out-of-process filters for:
- Language-agnostic filter implementations
- Isolated security-sensitive filters
- Third-party policy engines

---

## Appendix: Netfilter Reference

The BPA filter design draws from Linux netfilter's architecture.

### Netfilter Hook Points

| Netfilter | BPA Analog | Position |
|-----------|------------|----------|
| PRE_ROUTING | Ingress | First touch, before routing |
| LOCAL_IN | Deliver | After routing, destined for local |
| LOCAL_OUT | Originate | Locally-generated |
| POST_ROUTING | Egress | Final egress point |
| FORWARD | — | No analog (multi-topology via metadata) |

### Key Patterns Adopted

1. **Strategic hook points** — Different filtering needs at different stages
2. **Separation of concerns** — ReadFilter vs WriteFilter (like filter vs mangle tables)
3. **Chain semantics** — `Drop` stops processing, `Continue` allows others to vote
4. **Composability** — Multiple independent filters at each hook

### Patterns Not Adopted

1. **Numeric priorities** — Replaced with explicit `after` dependencies (self-documenting, no collisions)
2. **Tables** — Single DAG per hook instead of table hierarchy
3. **FORWARD hook** — Multi-topology routing handled via metadata, not a filter hook

### References

- [DigitalOcean: Deep Dive into Iptables and Netfilter Architecture](https://www.digitalocean.com/community/tutorials/a-deep-dive-into-iptables-and-netfilter-architecture)
- [nftables wiki: Netfilter hooks](https://wiki.nftables.org/wiki-nftables/index.php/Netfilter_hooks)
