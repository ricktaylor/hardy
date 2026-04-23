# BPA Filter Subsystem Design

## Overview

The BPA filter subsystem provides hook points at strategic locations in the bundle processing pipeline, allowing pluggable filters for security, policy enforcement, flow classification, and bundle modification.

The design draws heavily from Linux netfilter's architecture—see the "Netfilter Reference" appendix for the patterns that influenced this design.

## Related Documents

- **[Routing Design](routing_subsystem_design.md)**: RIB lookup and forwarding decisions that determine which filter hooks run
- **[Bundle State Machine Design](bundle_state_machine_design.md)**: Bundle status transitions and filter checkpoint semantics
- **[Policy Subsystem Design](policy_subsystem_design.md)**: Ingress filters can set flow_label for queue classification
- **[Storage Subsystem Design](storage_subsystem_design.md)**: Filter persistence

---

## Design Summary

| Aspect | Decision |
|--------|----------|
| **Hooks** | 4 hooks: Ingress, Deliver, Originate, Egress |
| **Filter Types** | 2 async traits: `ReadFilter` (read-only), `WriteFilter` (read-write) |
| **Registration** | `Filter` enum with trait object variants, `register(hook, name, after, filter)` |
| **Ordering** | Level-based via `after` dependencies (not numeric priorities) |
| **Parallelism** | ReadFilters: parallel within a level; WriteFilters: sequential |
| **Execution** | `FilterChainBuilder` per hook; pre-built `FilterChain` shared via `Arc` for lock-free async |
| **Result Semantics** | `Continue` = "no objection"; `Drop` = veto (stops processing) |

**Hook naming:**

- **Ingress** / **Egress**: Network boundary (CLA)
- **Deliver** / **Originate**: Service boundary

---

## Design Rationale

### Why Four Hooks?

The four hooks map to the natural decision boundaries in bundle processing:

- **Ingress**: First opportunity to reject invalid or malicious bundles before wasting resources on routing or storage. This is where size limits, source validation, and early policy checks belong.
- **Deliver**: Policy decisions that depend on the routing outcome (see [Routing Design](routing_subsystem_design.md) for RIB lookup details). For example, "allow delivery to service X but not Y" can only be evaluated after the RIB returns `FindResult::Deliver`.
- **Originate**: Enforce policy on locally-generated bundles before they enter the system. Services may attempt to send bundles that violate policy; this hook catches them early.
- **Egress**: Final validation and modification before network transmission. This is the last chance to add security blocks, validate the final bundle state, or log outbound traffic.

A FORWARD hook (like netfilter) was considered but rejected. In IP networking, forwarded packets take a different code path than locally-destined packets. In BPA, all bundles flow through the same dispatcher regardless of destination. Multi-topology routing is better handled via metadata-driven route selection than a separate filter hook.

### Why Two Filter Traits?

Separating `ReadFilter` and `WriteFilter` enables different execution models optimised for each use case:

**ReadFilters** only inspect bundles and return a pass/fail verdict. Because they don't modify state, multiple ReadFilters can safely run in parallel. This improves throughput for common validation tasks like size checks, ACL lookups, and source validation.

**WriteFilters** may modify the bundle bytes or metadata. These modifications must be serialised—each WriteFilter needs to see the result of previous modifications. Running them in parallel would create race conditions and non-deterministic results.

This separation mirrors netfilter's distinction between the `filter` table (accept/drop decisions) and `mangle` table (packet modification). The key insight is that read-only operations can be parallelised, while mutations require ordering.

### Why Dependency-Based Ordering?

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

### Why Pre-Built Chains?

The filter registry uses a sync `RwLock` to protect filter storage. This creates a problem for async execution:

1. **Send-safety**: `std::sync::RwLockReadGuard` is not `Send`. Holding it across `.await` points would make the future non-Send, incompatible with multi-threaded async runtimes like Tokio.
2. **Writer starvation**: If filters hold the read lock during execution (which may take milliseconds for gRPC calls), registration and unregistration operations would be blocked for extended periods.

The solution is to pre-build immutable `FilterChain`s and share them via `Arc`. The registry rebuilds the chains only when filters are registered or removed. On each `exec` call, the read lock is held only long enough to clone the `Arc` (one atomic increment), then execution proceeds without any lock.

This keeps lock hold times in the nanosecond range regardless of filter count or execution time, and produces a Send-safe future suitable for any async runtime.

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

## Filter Traits

See `bpa/src/filters/mod.rs` for trait definitions and result types.

Two async traits with identical signatures, differing only in return type:

- **`ReadFilter`**: Read-only inspection, returns `ReadResult` (`Continue` or `Drop`)
- **`WriteFilter`**: May modify bundle, returns `WriteResult` with optional new metadata/data

The `WriteResult::Continue` variant carries optional modifications:

- `(None, None)` — no change
- `(Some(meta), None)` — metadata changed, bundle bytes unchanged
- `(None, Some(data))` — bundle bytes changed (rare)
- `(Some(meta), Some(data))` — both changed

### Persistence

The bundle and data flow through the filter chain. WriteFilters mutate the bundle in place. The `ExecResult` carries the final bundle, data, and a `Mutation` struct that tracks whether any WriteFilter modified the data or metadata.

Persistence depends on the hook (see [Bundle State Machine Design](bundle_state_machine_design.md) for checkpoint semantics):

| Hook | Persistence Strategy |
|------|---------------------|
| **Ingress** | Persist bundle data only if `mutation.data` is true (in-place overwrite), then checkpoint to `Dispatching` status |
| **Originate** | No persistence (bundle stored after filter) |
| **Deliver** | No persistence (bundle consumed immediately after) |
| **Egress** | No persistence (bundle leaving node, may re-run on retry) |

---

## Registration API

See `bpa/src/filters/mod.rs` for the `Filter` enum, `Hook` enum, and `ExecResult` type, and `bpa/src/filters/registry.rs` for the `Registry`.

The `Filter` enum wraps either a `ReadFilter` or `WriteFilter` trait object in an `Arc`. The `Hook` enum identifies which hook point to register at.

### Registry Methods

- **`register(hook, name, after, filter)`** — Add a filter with explicit dependencies
- **`unregister(hook, name)`** — Remove a filter (fails if other filters depend on it)
- **`exec(hook, ...)`** — Execute all filters at a hook point

### Public API via `Bpa`

See `bpa/src/bpa.rs:register_filter()` and `unregister_filter()` for the public interface.

Filters are registered with a unique name and optional `after` dependencies. Filter names must be unique within a hook (not globally), since each hook maintains its own `FilterChainBuilder` and `after` dependencies are resolved per-hook. Unregistration checks for dependants and fails if other filters would be orphaned.

---

## Execution Model

### FilterChainBuilder and FilterChain

Each hook has a `FilterChainBuilder` for registration and a `FilterChain` for execution. The builder holds a flat `Vec<LevelBuilder>` where each level groups filters with no mutual dependencies. At registration time, a filter is placed at the level after the last level containing one of its dependencies. When filters are registered or removed, the builder produces a new immutable `FilterChain` containing only the filter references, stripped of registration metadata.

Filters declare dependencies via `after`. The system:

1. Resolves dependencies at registration time
2. Groups filters into levels (same dependencies satisfied)
3. Runs ReadFilters in parallel within a level
4. Runs WriteFilters sequentially within a level
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

The registry holds pre-built `FilterChain`s wrapped in an `Arc`. On each `exec` call, the read lock is held only long enough to clone the `Arc` (one atomic increment), then released. Execution proceeds without any lock held.

This prevents writer starvation, is Send-safe for async runtimes, and avoids per-bundle rebuilds of the execution structure.

### Rate-Limited Execution

Filter execution occurs within the dispatcher's `processing_pool` (a `BoundedTaskPool`). This:

- Prevents unbounded parallelism from exhausting system resources
- Applies backpressure when the pool is saturated
- Is configurable via `processing_pool_size` (default: 4 × available CPU cores)

The pool is shared across all bundle processing work (ingress, filter execution, dispatch).

### Parallelism Rules

| Trait | Parallelism |
|-------|-------------|
| **ReadFilter** | Parallel with other ReadFilters at same level |
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
                    ├─ persist data + checkpoint to Dispatching
                    └─▶ process_bundle(bundle)
                          ├─ RIB lookup (see routing_subsystem_design.md)
                          ├─ Deliver:
                          │     ├─ ◀── HOOK: Deliver
                          │     └─ deliver_bundle(service)
                          │           └─ (no persist - bundle dropped after delivery)
                          └─ Forward → egress path

Local origination:
  └─▶ local_dispatch(...)
        ├─ Builder::build() or CheckedBundle::parse()
        ├─ ◀── HOOK: Originate (in-memory, may set flow_label)
        ├─ store.store(bundle, data)  ← store AFTER filter
        └─▶ ingest_bundle(bundle)

Status reports (internal bundles, skip Originate):
  └─▶ dispatch_status_report(...)
        ├─ Builder::build()
        ├─ store.store(bundle, data)
        └─▶ ingest_bundle_inner(bundle)  ← runs Ingress filter

Egress path:
  └─▶ forward_bundle(bundle)  ← after dequeue from ForwardPending
        ├─ load bundle data
        ├─ update extension blocks (hop count, previous node, bundle age)
        ├─ ◀── HOOK: Egress (in-memory only, like Deliver)
        └─ CLA.send()
```

**Filter-then-store pattern:** For Originate hooks, the filter runs on an in-memory bundle before storing. If the filter drops the bundle, nothing is persisted. Filter modifications (e.g., flow_label) are preserved in the single store operation.

### Hook Placement

| Hook | Position | Use Cases |
|------|----------|-----------|
| **Ingress** | After parse, before routing | Size limits, source validation, flow classification |
| **Deliver** | After RIB "Deliver", before service | Service access control, metadata injection |
| **Originate** | Before store, in-memory | Source policy, flow label (caller handles crash/retry) |
| **Egress** | After dequeue, before CLA send | BPSec (BIB/BCB), final validation, logging |

---

## Typical Filter Usage

| Hook | ReadFilter | WriteFilter |
|------|------------|-------------|
| **Ingress** | Size limits, source validation | Flow classification, storage policy |
| **Deliver** | Service access control | Add extension blocks |
| **Originate** | Source policy enforcement | Flow label, add BIB |
| **Egress** | Final validation | Add BIB, BCB |

### Built-in Filters

| Name | Trait | Purpose |
|------|-------|---------|
| `rfc9171-validity` | Read | RFC 9171 validity checks (see below) |
| `ipn-legacy` | Write | Rewrite IPN EIDs to legacy encoding |

### Example Filter Ideas

The following are examples of filters that could be implemented:

| Name | Trait | Purpose |
|------|-------|---------|
| `size_check` | Read | Reject oversized bundles |
| `source_validator` | Read | Validate bundle source against policy |
| `destination_acl` | Read | Enforce destination access control |
| `flow_classifier` | Write | Set flow label for queue classification (see [Policy Subsystem Design](policy_subsystem_design.md)) |
| `add_bib` | Write | Add Bundle Integrity Block |
| `add_bcb` | Write | Add Bundle Confidentiality Block |

### RFC 9171 Validity Filter

The `rfc9171-validity` filter enforces policy requirements from RFC 9171 at the Ingress hook. These checks are configurable because they represent policy decisions rather than structural validity (which is enforced during parsing in hardy-bpv7).

**Configurable checks:**

- **Primary block integrity** (`primary-block-integrity`, default: `true`): Requires that the primary block is protected by either a CRC or a Bundle Integrity Block (BIB). RFC 9171 §4.3.1 recommends this protection but does not mandate it. Disable for interoperability with implementations that omit primary block CRCs.

- **Bundle Age required** (`bundle-age-required`, default: `true`): Requires a Bundle Age block when the creation timestamp is zero (indicating no clock). RFC 9171 §4.2.7 makes this a SHOULD requirement. The check ensures bundles can be aged and expired correctly.

**Auto-registration:**

The filter is auto-registered by default with default configuration (both checks enabled). Applications that need custom configuration should:

1. Enable the `no-rfc9171-autoregister` feature flag
2. Register the filter manually with the desired configuration

This pattern allows the library to provide sensible defaults while giving applications full control when needed.

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
2. **Tables** — Single leveled chain per hook instead of table hierarchy
3. **FORWARD hook** — Multi-topology routing handled via metadata, not a filter hook

### References

- [DigitalOcean: Deep Dive into Iptables and Netfilter Architecture](https://www.digitalocean.com/community/tutorials/a-deep-dive-into-iptables-and-netfilter-architecture)
- [nftables wiki: Netfilter hooks](https://wiki.nftables.org/wiki-nftables/index.php/Netfilter_hooks)
