# hardy-async

Async runtime abstraction for the Hardy DTN implementation.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Installation

```toml
[dependencies]
hardy-async = "0.1"
```

Published on [crates.io](https://crates.io/crates/hardy-async).

## Overview

hardy-async provides runtime-agnostic async primitives used by all Hardy crates that spawn tasks or require synchronisation. It abstracts over tokio today, with the architecture designed for future no\_std/Embassy support via feature flags. By routing all async primitive usage through this crate, switching runtimes becomes a feature flag change rather than a codebase-wide refactor.

## Features

- **TaskPool** -- manages cancellable tasks with three-phase graceful shutdown (signal, close, wait). Derives `Clone` for shared ownership across components
- **BoundedTaskPool** -- extends TaskPool with semaphore-based concurrency limiting and backpressure. Default limit matches available CPU parallelism
- **`spawn!` macro** -- tracing-instrumented task spawning with `follows_from` span relationships
- **Signal handling** -- `signal::listen_for_cancel(&TaskPool)` registers SIGTERM and Ctrl+C handlers
- **Hierarchical cancellation** -- `child_token()` creates tokens that cancel independently but cascade from parent shutdown
- **Synchronisation primitives**:
  - `sync::Mutex` / `sync::RwLock` -- general-purpose locks for O(n) or blocking operations
  - `sync::spin::Mutex` / `sync::spin::RwLock` -- spinlock-based locks for O(1) hot-path operations
  - `sync::spin::Once` -- one-time lazy initialisation cell
  - `Notify` -- task notification primitive
- **Time utilities** -- runtime-agnostic `sleep()` with edge-case handling (negative durations, overflow)
- **CancellationToken** / **JoinHandle** -- type aliases abstracting runtime-specific primitives

### Feature Flags

- **`tokio`** (default) -- Tokio runtime backend. Implies `std`
- **`std`** -- Enables std-based sync primitives and OS thread count queries
- **`instrument`** -- Enables span instrumentation in the `spawn!` macro via `tracing/attributes`

## Usage

```rust
use hardy_async::TaskPool;

let pool = TaskPool::new();
let cancel = pool.cancel_token().clone();

pool.spawn(async move {
    loop {
        tokio::select! {
            _ = do_work() => {}
            _ = cancel.cancelled() => break,
        }
    }
});

// Later: graceful shutdown
pool.shutdown().await;
```

Bounded concurrency:

```rust
use hardy_async::BoundedTaskPool;

// At most 4 concurrent tasks; spawn() awaits a permit
let pool = BoundedTaskPool::new(core::num::NonZeroUsize::new(4).unwrap());

for i in 0..100 {
    pool.spawn(async move {
        process_item(i).await;
    }).await;
}

pool.shutdown().await;
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [API Documentation](https://docs.rs/hardy-async)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
