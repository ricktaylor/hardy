# hardy-async Design

Runtime-agnostic async primitives for Hardy components.

## Design Goals

- **Runtime portability.** The core purpose of this library is enabling Hardy components to run on different async runtimes without code changes. By routing all async primitive usage through hardy-async, switching between Tokio (cloud deployment), smol (lightweight), or Embassy (`no_std` embedded) becomes a feature flag change rather than a codebase-wide refactor.

- **`no_std` support.** DTN nodes range from cloud servers to constrained embedded devices. This library is designed from the ground up to support `no_std` environments via the Embassy runtime, requiring only a heap allocator. Currently the Tokio backend is implemented; Embassy support is planned.

- **Graceful shutdown.** Encapsulate the common pattern of signalling cancellation, preventing new work, and waiting for existing work to complete. This three-phase shutdown is error-prone when implemented ad-hoc across multiple components.

- **Bounded concurrency.** Support limiting the number of concurrent tasks to prevent resource exhaustion, with backpressure that causes spawners to wait when the pool is at capacity.

## Task Pool Pattern

The `TaskPool` type encapsulates the relationship between task spawning, cancellation, and shutdown. Rather than separately managing a cancellation token and a task tracker, `TaskPool` combines them with coordinated semantics.

Shutdown follows a three-phase pattern. First, all tasks receive a cancellation signal via the shared token. Second, the pool closes to prevent new task spawning. Third, the pool waits for all existing tasks to complete. Tasks are expected to check the cancellation token periodically and exit gracefully when signalled.

This pattern appears throughout Hardy - every long-running service needs coordinated shutdown. Extracting it to a shared primitive ensures consistent behaviour and reduces the chance of shutdown-related bugs like orphaned tasks or deadlocks.

### Hierarchical Cancellation

The `child_token()` method creates tokens that can be cancelled independently of their parent. Cancelling a child token doesn't affect the parent pool or sibling tokens. However, when the parent pool shuts down, all child tokens are automatically cancelled.

This hierarchy supports scenarios where a service spawns subtasks that might need individual cancellation (e.g., timing out a slow operation) without affecting the overall service lifecycle.

## Bounded Task Pool

`BoundedTaskPool` extends the task pool pattern with a semaphore-based concurrency limit. When the configured number of tasks are already running, further spawn calls block until a slot becomes available.

This provides natural backpressure. A component processing a stream of incoming bundles won't spawn unbounded tasks - it will slow down when the processing pool is saturated. The limit prevents memory exhaustion from queued work and keeps resource usage predictable.

The default concurrency limit matches the available CPU parallelism, which is sensible for CPU-bound work but can be overridden for I/O-bound tasks or resource-constrained environments.

## Spawn Macro

The `spawn!` macro provides a convenient syntax for spawning tasks with optional tracing instrumentation. When the `tracing` feature is enabled, spawned tasks automatically get a span with a `follows_from` relationship to the spawning context.

This addresses a common problem in async code: when work moves across task boundaries, the causal relationship between tasks can be lost in traces. The macro ensures spawn points are consistently instrumented without requiring manual span management at every call site.

## Synchronization Primitives

The `sync` module provides synchronization primitives with a unified API across platforms, abstracting over std and future Embassy implementations.

Two tiers of primitives serve different use cases:

| Use Case | Primitive |
|----------|-----------|
| O(1) ops, hot path, no blocking | `sync::spin::Mutex`, `sync::spin::RwLock` |
| O(n) iteration, may block or syscall | `sync::Mutex`, `sync::RwLock` |

Spinlock-based primitives are appropriate for quick O(1) operations where locks are held briefly. The std-based primitives handle poison errors via `trace_expect()`, providing a unified interface that matches Embassy's semantics (no poison concept) while logging poisoning events before panicking.

The `sync::spin::Once` primitive supports lazy one-time initialization for configuration or singleton patterns.

## Time Utilities

The `time` module provides runtime-agnostic time operations. The `sleep()` function wraps runtime-specific timers, handling edge cases like negative durations (returns immediately) and durations exceeding platform limits.

The `Notify` type abstracts over runtime-specific notification primitives for signaling between tasks.

## Runtime Abstraction

Most types in this library are thin wrappers or type aliases over runtime-specific primitives. For the Tokio backend, `CancellationToken` and `JoinHandle` alias tokio-util types directly.

The abstraction adds minimal overhead - it's primarily organisational. Each runtime backend provides the same API surface, so consuming code remains unchanged when switching runtimes via feature flags.

## Integration

### With Hardy Components

All Hardy services that spawn long-running tasks use `TaskPool` for lifecycle management. This includes the BPA's bundle processing workers, CLA connection handlers, and background maintenance tasks.

The consistent pattern means shutdown signals propagate uniformly through the system. When the BPA receives SIGTERM, its task pool cascades cancellation to all subsystems.

### With Observability

When the `tracing` feature is enabled, the `spawn!` macro integrates with the distributed tracing infrastructure. This provides visibility into task relationships and helps diagnose concurrency issues in production.

## Dependencies

Feature flags control runtime backend and optional capabilities:

- **`tokio`** (default): Tokio runtime backend. Implies `std`.
- **`std`**: Enables std-based synchronization primitives and system thread count queries.
- **`tracing`**: Enables span instrumentation in the `spawn!` macro.

Key external dependencies:

| Crate | Purpose |
|-------|---------|
| tokio | Async runtime (optional) |
| tokio-util | CancellationToken, task tracking (optional) |
| spin | Spinlock primitives for `no_std`-compatible sync |
| trace-err | Poison error handling with tracing integration |
| async-trait | Async trait support |
| time | Duration types |

## Testing

The library includes tests for concurrency limiting behaviour and shutdown semantics. Task pools are straightforward to test because shutdown is deterministic - once signalled, tasks complete in bounded time.
