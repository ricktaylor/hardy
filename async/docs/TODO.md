# hardy-async TODO

## `GatedTaskPool`: non-blocking bounded spawn

### Background

`BoundedTaskPool::spawn` (`src/bounded_task_pool.rs`) acquires a semaphore permit in the caller before spawning: a saturated pool parks the *caller*, and the wait is not cancel-aware. That shape is wrong for callers that must never block on pool capacity. The canonical case is `RpcProxy`'s stream reader (`proto/src/proxy.rs` on `fix/concurrent-delivery-stalls`), which demultiplexes responses and requests off one gRPC stream: if the reader parks on handler capacity it cannot read the very responses whose completion frees the pool — a deadlock class that has already flipped direction twice on that branch.

The proxy therefore hand-rolls the needed semantics inline with a plain `TaskPool` + `Arc<Semaphore>`: spawn the handler task immediately, acquire the permit *inside* the task raced against the pool's cancel token, so submission never blocks and parked tasks abandon the wait on shutdown. This is correct, but the concurrency bound is enforced by one spawn site's discipline rather than by a type — any future spawn on the same pool that skips the permit silently unbounds handler concurrency and reopens the deadlock class.

### Design constraints

- **The `spawn!` macro.** All task spawning goes through `spawn!` (`src/spawn.rs`), which is duck-typed on `$pool.spawn(instrumented_future)` — a differently-named method sidesteps it, and a parallel macro per spawn flavour does not scale (the reverted `spawn_with_permit!` from `d23491de` was that shape). The queued entry point must therefore be the `spawn` method of some type the macro can take.
- **The type must not also offer the caller-blocking flavour.** An adapter view on `BoundedTaskPool` (e.g. `pool.queued()`) satisfies the macro but leaves the caller-parks `spawn` available on the same value — the wrong choice stays one keystroke away, which is the discipline problem this item exists to remove.

Hence a dedicated type, `GatedTaskPool`: internals near-identical to `BoundedTaskPool` (`TaskPool` + `Arc<Semaphore>`), but its only `spawn` is the gated one, so the semantics are unmistakable at the field declaration and `spawn!(pool, "name", async ...)` call sites need nothing extra. Its `spawn` is synchronous (spawns immediately, returns the handle), which composes with `spawn!` more cleanly than `BoundedTaskPool::spawn`, whose caller-side permit wait forces an `.await` onto macro call sites.

Deliberately a standalone struct, **not** a newtype wrapping `BoundedTaskPool`: a wrapper would exist to suppress the wrapped type's defining method while inheriting its future semantic changes (semaphore lifecycle, default bound), coupling two contracts that should evolve independently — and it invites a convenience `Deref` that would re-expose the blocking `spawn`. The duplicated substance is two fields, a constructor, and one-line accessor delegations; both shapes optimize identically (single-field wrappers are zero-cost), so coupling, not codegen, is the deciding axis.

The two bounded pools then have honestly distinct contracts, worth stating in both rustdocs: `BoundedTaskPool` bounds *submission* (the caller waits — backpressure, as used by the bpa dispatcher and filter chain); `GatedTaskPool` bounds *execution* (the caller never waits; queued tasks hold a slot in the pool, and their permit wait races the cancel token). Queue depth is unbounded by design — bounding it would reintroduce caller blocking; callers needing inbound backpressure must provide it at their own layer.

With this shape the tracing span wraps the permit wait, so time spent queued is attributed to the task's span — matching the hand-rolled code's behaviour, and useful in traces.

### What's needed

- `GatedTaskPool` with `spawn(task)`: spawns immediately, waits for a permit inside the spawned task, and races that wait against the pool's cancel token (the task exits without running if cancelled first). Document the contract — submission never blocks; queued tasks drain on shutdown — and unit-test both properties.
- A construction path that shares an existing `TaskPool`'s cancel domain, so `RpcProxy` can keep its reader/writer alongside the throttled handlers (e.g. `GatedTaskPool::sharing(&TaskPool, NonZeroUsize)` in addition to a self-contained `new`). Decide lifecycle ownership: a sharing-constructed pool probably should not expose `shutdown()` — that belongs to the owner of the shared `TaskPool`.
- Port `proto/src/proxy.rs` to the new type once `fix/concurrent-delivery-stalls` has merged, replacing the inline `TaskPool` + `Semaphore` pair in `Reader`. Residual review point that the type cannot catch: the proxy holds both pool handles, so a handler mistakenly spawned on the infra `TaskPool` is still possible — but it is a visible cross-type error rather than an omitted permit.
- When porting, reconsider the bound the proxy passes: `available_parallelism()` suits CPU-bound work, but queued-spawn callers are typically IO-bound (RPC handlers awaiting round-trips), where a cores-derived per-connection throttle is arbitrary — on a 1-core host it serializes deliveries per connection.
