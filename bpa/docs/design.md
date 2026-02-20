# hardy-bpa Design

Core bundle processing agent library implementing RFC 9171.

## Design Goals

- **Library-first architecture.** The BPA is a library, not an application. Applications like hardy-bpa-server embed the library and provide configuration, network binding, and integration with external systems. This separation allows the same BPA logic to run in different deployment contexts.

- **Trait-based extensibility.** Storage backends, convergence layer adaptors, application services, and egress policies are all defined as traits. Concrete implementations are injected at runtime, allowing operators to swap components without modifying core BPA code.

- **Parallel pipeline processing.** Bundle processing is parallelised across a bounded task pool. The pool size limits concurrent processing to prevent resource exhaustion while ensuring bundles don't queue indefinitely behind slow operations.

- **Crash-resilient storage.** Bundles must not be lost due to crashes or restarts. The storage subsystem persists bundle data before acknowledging receipt, uses status checkpoints to track processing progress, and recovers in-flight bundles on restart. This ensures bundles are never silently dropped, even during unexpected termination.

- **Zero-copy bundle handling.** Bundle payloads can be large (megabytes). The BPA uses `Bytes` (reference-counted byte buffers) throughout the pipeline, enabling bundle data to flow from ingress through storage to egress without copying. Slicing creates views into the same underlying buffer.

- **Dynamic runtime reconfiguration.** Routes, CLAs, services, and filters can be registered and unregistered at runtime without restart. This enables the BPA to adapt to changing network conditions—scheduled contacts, new peer discovery, and policy updates—without service interruption.

## Architecture Overview

The BPA processes bundles through a pipeline with several stages. Each bundle flows from ingress through routing decisions to either local delivery or forwarding.

```
Ingress → Validation → Filtering → Routing → Storage/Dispatch → Egress
```

The `Bpa` struct coordinates the major subsystems:

- **Store** coordinates data and metadata persistence with caching
- **RIB** maintains routing rules and triggers re-evaluation on changes
- **Dispatcher** the central processing hub (see below)
- **Registries** manage CLAs, services, filters, and keys

### Dispatcher as Central Hub

The `Dispatcher` is the central coordinator that orchestrates all bundle processing. It holds references to every registry and subsystem, routing bundles through the appropriate stages based on their state and destination.

```
                              ┌────────────────────────────────────┐
                              │            Dispatcher              │
                              │                                    │
   CLA Ingress ──────────────►│  ┌─────────┐      ┌─────────────┐  │
                              │  │ Filter  │      │   Service   │  │
   Service Egress ───────────►│  │Registry │      │  Registry   │  │
                              │  └─────────┘      └─────────────┘  │
                              │                                    │
                              │  ┌─────────┐      ┌─────────────┐  │
   Storage ◄─────────────────►│  │  Store  │      │     RIB     │  │
                              │  └─────────┘      └─────────────┘  │
                              │                                    │
                              │  ┌─────────┐      ┌─────────────┐  │
   CLA Egress ◄───────────────┤  │   CLA   │      │    Keys     │  │
                              │  │Registry │      │  Registry   │  │
                              │  └─────────┘      └─────────────┘  │
                              └────────────────────────────────────┘
```

This centralised design ensures consistent bundle handling across all paths (CLA ingress, service egress, restart recovery) while keeping subsystems decoupled from each other.

### Bundle Flow

A bundle entering from a CLA follows this path:

1. **Ingress**: CLA calls `Sink::dispatch()` with raw bytes and peer information
2. **Validation**: `RewrittenBundle::parse()` performs full validation and canonicalisation
3. **Filtering**: Ingress filters may drop, modify, or mark the bundle
4. **Storage**: Bundle persisted with `New` status before processing continues
5. **Dispatch**: Destination examined - local delivery, admin endpoint, or forwarding
6. **Routing**: RIB lookup determines next hop for forwarding bundles
7. **Egress**: Bundle queued to CLA for transmission, egress filters applied

Failed bundles generate status reports where requested and permitted.

## Subsystem Design

The BPA's complexity is distributed across several subsystems, each documented separately:

- **[Storage Subsystem](storage_subsystem_design.md)** - Dual storage model with separate data and metadata backends, LRU caching, crash recovery, and expiration monitoring
- **[Bundle State Machine](bundle_state_machine_design.md)** - Bundle lifecycle states and transitions that serve as crash recovery checkpoints
- **[Routing](routing_subsystem_design.md)** - RIB structure, pattern matching, route priorities, and forwarding decisions
- **[Filter Subsystem](filter_subsystem_design.md)** - Hook points, filter ordering, and traffic modification
- **[Policy Subsystem](policy_subsystem_design.md)** - Egress queue management, traffic classification, and rate limiting

## Key Design Decisions

### Dual Storage Model

Bundle data and metadata are stored in separate backends with fundamentally different characteristics:

| Aspect | Metadata Storage | Bundle Storage |
|--------|------------------|----------------|
| Access pattern | Frequent queries, updates | Rare access (forward/deliver) |
| Data model | Relational (indexed by status, expiry, queue) | Blob (keyed by storage name) |
| Size | Small (hundreds of bytes per bundle) | Large (potentially megabytes) |
| Examples | hardy-sqlite-storage, future: PostgreSQL | hardy-localdisk-storage, future: S3 |

This separation enables independent backend selection optimised for each access pattern. Metadata benefits from relational indexing for efficient status queries and queue management. Bundle data benefits from blob storage with optional memory mapping for large payloads.

See the storage backend packages for production implementations:

- [hardy-sqlite-storage](../../sqlite-storage/docs/design.md) - SQLite-based metadata storage
- [hardy-localdisk-storage](../../localdisk-storage/docs/design.md) - Filesystem-based bundle storage

### Application vs Service APIs

Two levels of service integration exist:

**Application** is the high-level API. Applications receive decoded payloads and send data that the BPA wraps in bundles. The Application API hides bundle structure, suitable for most user services.

**Service** is the low-level API. Services receive raw bundle bytes and construct bundles themselves using the Builder. The BPA still validates outbound bundles (services are not trusted), but this API enables system services like echo that need to inspect or modify bundle structure.

### Component Registry and Sink Pattern

External components (CLAs, services, future routing agents) are managed through a consistent architectural pattern combining Registries with paired traits.

#### Registry Pattern

Each component type has a dedicated Registry that manages registration, lifecycle coordination, and component-specific state. Registries create Sinks with weak back-references to avoid reference cycles.

| Registry | Component Trait | Sink Trait | Purpose |
|----------|-----------------|------------|---------|
| `cla::registry::Registry` | `Cla` | `cla::Sink` | Convergence layer adapters |
| `services::registry::Registry` | `Service` | `ServiceSink` | Low-level bundle services |
| `services::registry::Registry` | `Application` | `ApplicationSink` | High-level payload services |
| `filters::registry::Registry` | `Filter` | — | Traffic filtering (no Sink needed) |
| `keys::registry::Registry` | `KeyProvider` | — | BPSec key management |
| (planned) | `RoutingAgent` | `RoutingSink` | Dynamic routing protocols |

#### Bidirectional Sink Pattern

Components communicate with the BPA through paired traits: a primary trait implemented by the component, and a corresponding Sink trait provided by the BPA. The Sink provides methods to interact with the BPA (dispatch bundles, send data, manage state) without holding direct references to BPA internals. This indirection creates a stable interface that enables independent evolution, isolated testing, and transparent local/remote operation via the [`BpaRegistration`] trait.

#### Lifecycle

The Registry holds a strong reference to registered components. Disconnection is bidirectional: components can call `sink.unregister()` or drop their Sink, and the BPA can initiate shutdown calling `on_unregister()`. After disconnection, the Sink returns `Disconnected` errors for all operations.

See the [`BpaRegistration`] trait documentation for implementation requirements and recommended patterns.

[`BpaRegistration`]: ../src/bpa.rs

#### Authorization and Ownership

The Sink pattern provides **structural authorization enforcement**. Each component receives a Sink bound to its own resources, preventing cross-component interference without explicit authorization tokens.

**How it works:**

1. **Per-registration Sink**: Each registered component gets its own Sink instance containing weak references to that component's resources (e.g., `Weak<Service>`, the CLA's peer map).

2. **Scoped operations**: Sink methods operate only on the bound resources:
   - `ServiceSink::unregister()` unregisters only the service it was created for
   - `ServiceSink::cancel()` validates `bundle_id.source == service.service_id`
   - `cla::Sink::remove_peer()` operates on the CLA's own peer map

3. **No cross-access possible**: A component cannot affect another component's resources because it has no reference to them.

This design means **no authorization token is required** for ownership enforcement—it's enforced by the object reference graph. A malicious or buggy component can only affect its own registrations.

For deployments requiring additional authorization (namespace restrictions, audit logging), the gRPC layer can add identity validation at registration time. See the [hardy-proto design](../../proto/docs/design.md#trust-model) for details.

### Routing Information Base

The RIB maintains routing rules as a priority-ordered collection of EID patterns mapping to actions. When a route changes, the RIB notifies a background task to re-evaluate bundles in `Waiting` status. This ensures bundles aren't stranded when new routes become available.

Routes are keyed by `(priority, pattern, action, source)` allowing multiple routes to the same destination through different CLAs. Priority ordering ensures deterministic selection when multiple routes match.

### Bounded Processing Pool

Bundle processing uses `BoundedTaskPool` with a configurable concurrency limit. When the pool is saturated, ingress naturally slows down - new bundles wait for processing slots rather than queuing unboundedly in memory.

This provides backpressure through the system. CLAs that receive bundles faster than the BPA can process them will experience slowdown at the dispatch call, which can propagate to their network handling.

### Storage-Backed Queues

Internal queues (dispatch queue, per-peer egress queues) use a hybrid architecture that combines in-memory channels with persistent storage fallback:

```
┌──────────────────┐
│  Fast Path       │  In-memory bounded channel
│  (channel open)  │  Sub-millisecond latency
└────────┬─────────┘
         │ channel full
         ▼
┌──────────────────┐
│  Slow Path       │  Spill to metadata storage
│  (draining)      │  Background poller refills channel
└──────────────────┘
```

When the in-memory channel has capacity, sends complete immediately. When full, bundles are persisted to metadata storage with appropriate `BundleStatus` (e.g., `ForwardPending { peer, queue }`). A background task drains storage back into the channel when space becomes available.

This design provides:

- **Bounded memory usage**: Queue depth is limited regardless of bundle arrival rate
- **Crash recovery**: Queued bundles survive restarts via their persisted status
- **Backpressure**: Storage insertion rate naturally limits ingress when overwhelmed
- **Low latency**: Fast path avoids storage I/O for normal operation

See [Policy Subsystem Design](policy_subsystem_design.md#hybrid-channel-architecture) for implementation details.

## Integration

### With hardy-bpv7

The BPA uses all three parsing modes:

- `RewrittenBundle` for CLA input (untrusted, full validation)
- `CheckedBundle` for service input (semi-trusted, canonicalisation only)
- `ParsedBundle` for quick routing inspection

### With Storage Backends

Storage traits (`BundleStorage`, `MetadataStorage`) are injected by the embedding application. The library includes in-memory implementations for testing; production deployments use hardy-localdisk-storage and hardy-sqlite-storage. The trait-based design enables future backends (S3, PostgreSQL) without BPA changes.

### With hardy-proto

The gRPC interfaces defined in hardy-proto enable distributed deployment where CLAs and services run as separate processes communicating with the BPA over gRPC.

### Observability

The BPA is instrumented for observability through two mechanisms:

**Tracing**: The `tracing` feature enables `#[instrument]` attributes on key methods, providing structured span data for distributed tracing. The embedding application (e.g., hardy-bpa-server with hardy-otel) configures the tracing subscriber.

**Metrics**: The `metrics` crate provides hooks for counters, gauges, and histograms. Key processing events emit metrics that the embedding application can export to monitoring systems. Current coverage is foundational; expanding metric coverage across all subsystems is ongoing work.

This separation keeps the BPA library independent of specific observability backends while providing the hooks needed for production monitoring.

## Configuration

The `Config` struct controls BPA behaviour:

| Option | Default | Purpose |
|--------|---------|---------|
| `status_reports` | `false` | Enable RFC 9171 bundle status report generation |
| `poll_channel_depth` | 16 | Capacity of internal dispatch channels before falling back to storage |
| `processing_pool_size` | 4 × CPU cores | Maximum concurrent bundle processing tasks |
| `storage_config` | — | LRU cache settings (capacity, max cached bundle size) |
| `node_ids` | — | Local node identifiers (IPN and/or DTN schemes) |

Storage backends (`metadata_storage`, `bundle_storage`) are injected programmatically rather than configured, allowing the embedding application to select appropriate backends for its deployment context.

## Future Work

### Key Provider Infrastructure

The `keys` module provides a `KeyProvider` trait and registry for BPSec key management. The current implementation aggregates keys from registered providers but lacks configuration-driven key loading. Future work includes:

- Loading keys from configuration files
- Integration with external key management systems (HSMs, Vault)
- Key rotation and expiration handling

### Storage Priority and Eviction

When storage capacity is exhausted, bundles must be evicted. The filter subsystem can assign a `storage_priority` to bundles during ingress, enabling priority-based eviction policies. This infrastructure is planned but not yet implemented.

## Dependencies

The library is `no_std` compatible with a heap allocator, though full support is currently blocked by `flume` and `metrics` which require `std`. See the crate documentation for embedded target requirements.

Feature flags control optional functionality:

- **`tokio`** (default): Tokio async runtime. Implies `std`.
- **`rfc9173`**: RFC 9173 default security contexts via hardy-bpv7.
- **`std`**: Standard library support for time and collections.
- **`serde`**: Serialization support for configuration and metadata.
- **`tracing`**: Span instrumentation for async tasks.
- **`no-rfc9171-autoregister`**: Disable auto-registration of the RFC 9171 validity filter. Use this when the embedding application needs to register the filter with custom configuration.

Key external dependencies:

| Crate | Purpose |
|-------|---------|
| hardy-async | Async runtime abstraction |
| hardy-bpv7 | Bundle protocol implementation |
| hardy-eid-patterns | EID pattern matching for routing |
| lru | LRU cache for bundle data |
| time | Timestamp handling |
| tracing | Instrumentation |
| metrics | Performance metrics |

## Testing

- [Unit Test Plan](unit_test_plan.md) - BPA internal algorithms (routing, policy)
- [Component Test Plan](../src/component_test_plan.md) - Pipeline integration, performance benchmarks
- [CLA Integration Tests](cla_integration_test_plan.md) - Generic CLA trait verification
- [Service Integration Tests](service_integration_test_plan.md) - Generic service trait verification
- [Storage Integration Tests](storage_integration_test_plan.md) - Generic storage trait verification
- [Fuzz Test Plan](fuzz_test_plan.md) - Async pipeline stability
