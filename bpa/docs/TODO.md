# bpa TODO

## Fragmentation and ADU reassembly

All fragmentation-shaped work — §5.9 material-extents reassembly, deletion reports on reassembly failure, fragment-carried payload BIB deferral, streaming-shaped reassembly — is consolidated in [`fixing_fragmentation.md`](fixing_fragmentation.md), pending a decision once the bulk of the streaming work lands.

## Ingress status-report conformance (invalid bundles, duplicates, expired arrivals)

RFC 9171 pins the reception report's reason code: §5.6 Step 2 prescribes "No additional information" (Step 4's "Block unsupported" is the only other reception-report reason), and a parse/CRC failure routes through the §5.10 Bundle Deletion procedure, whose deletion report is what cites "Block unintelligible". The ingress invalid-bundle path instead sends a single reception report carrying the failure reason and no deletion report. This shape is shared by main and refactor/cla-streaming, and survives on refactor/parse and refactor/metadata in both invalid paths there (`parse_headers` failure and post-drain `finalize` failure) — only the lifetime/hop early gate on those branches emits the conforming reception-then-deletion pair. Fix when the parse refactoring is picked up post-v0.2.0: reception report with "No additional information", deletion report citing the failure reason.

Duplicates: RFC 9171 specifies no duplicate-bundle processing at all (the word does not appear), so §5.6 read literally reports reception on every arrival, including replays. Decision: duplicates SHOULD get a reception report — a sender may be intentionally repeating a bundle probing for status-report ACKs. refactor/parse already reports reception before the duplicate check; main reports after, so a refused duplicate is silent. Adopt the report-before-dedup ordering with the refactor.

Expired arrivals are the deliberate exception: a bundle that arrives already expired is treated as if it never arrived — no reception report, no deletion report, no metadata entry — rather than amplifying already-dead traffic into report bundles. The pre-drain early gate implements this silence for the lifetime case (hop-exhaustion still emits the reception-then-deletion pair). Bundles that expire *in custody* are unaffected: the validity filter, reaper, and `drop_bundle` paths still generate §5.10 deletion reports citing "Lifetime expired".

## Storage backend metrics (common + bespoke)

An audit (2026-07-10, while defaulting bpa-server to sqlite/localdisk) found the persistent storage backends emit no metrics at all — none of sqlite-storage, localdisk-storage, postgres-storage, or s3-storage even depends on the `metrics` crate. Their only observability is the `instrument`-feature tracing spans. Production nodes on the new persistent defaults therefore have no storage operation counts, error counters, or latencies; only the in-memory backends carry store-level metrics today (`bpa.mem_store.*`, `bpa.mem_metadata.*`, documented in `docs/user-docs/operations/observability.md`).

Two layers of work:

**Common metrics in the `Store` wrapper** (`src/storage/store.rs`) — every operation flows through it, so instrumenting there covers all backends uniformly and keeps the backend crates metrics-free for the common cases. Proposed: `bpa.storage.ops` (counter), `bpa.storage.errors` (counter), and `bpa.storage.op.duration` (histogram), labelled `store = metadata|bundle` and `op = load|save|replace|delete|insert|tombstone|get|poll` (the `poll_*` variants folded into one label value to bound cardinality).

**Bespoke per-backend metrics** where only the backend knows the truth: sqlite database/WAL file size, localdisk store-directory bytes and file count, postgres connection-pool stats, s3 request retries. These need either per-crate `metrics` emission or a `stats()`-style trait extension sampled periodically — a trait-surface decision to make when the work is picked up.

Documentation: a new backend-agnostic "Storage Operations" table in `docs/user-docs/operations/observability.md`, plus per-backend tables as bespoke metrics land.

## Registration/routing concurrency races (whole-codebase review 2026-07-08, #14/#15)

Two non-atomic await sequences in the CLA/routing registries can race and leave stranded or wrongly-deleted state. Neither is data-loss or wire-corruption, and both need specific concurrent timing, so they were deferred past v0.2.0.

**`add_peer` vs concurrent removal (`src/cla/registry.rs`, review #14).** `add_peer` is a multi-await sequence (PeerTable insert → `cla.peers` insert → `peer.start().await` → `rib.add_forward().await`) with no re-check against a concurrent `remove_peer`/`unregister_cla` interleaving during the awaits. If the peer is removed mid-sequence, `add_peer` resumes and installs priority-0 `Forward` RIB entries for a `peer_id` no longer in the PeerTable, and nothing later cleans them (`unregister_cla` only iterates `cla.peers`, which no longer holds the address). Mirror race leaks a live PeerTable entry after CLA teardown. Fix needs a post-await liveness re-check (or a generation/epoch guard) before `rib.add_forward`.

**`unregister_agent` name-reuse race (`src/routing/rib.rs`, review #15).** `unregister_agent` removes the agent from the name map, `await`s `agent.on_unregister()` (arbitrary duration — e.g. a gRPC proxy drain), then calls `remove_by_source(name)`. A new agent that registers under the same name during that await (the map slot is already free) has its freshly-installed routes deleted by the stale `remove_by_source`, which matches purely on the source string. Fix needs an identity/generation token so `remove_by_source` only removes routes owned by the unregistering agent instance, not a same-named successor.

## Sink drop-cleanup via reconciler queue (replace spawn-from-Drop)

All three Sink `Drop` impls (`src/cla/registry.rs`, `src/services/registry.rs`, `src/routing/agent/sink.rs`) enqueue their async unregistration by spawning a task from sync `Drop`. This is already non-blocking, but spawn-from-Drop allocates a task per drop, gets exactly-once semantics only from ad-hoc guards, and can silently not run when the pool is already cancelled during shutdown — only the routing sink checks `is_cancelled`.

The tidy-up: `Drop` pushes a component identity onto an unbounded channel drained by one long-lived reconciler per registry — `Drop` stays plain sync code, delivery is exactly-once (an explicit `unregister()` leaves a stale id the reconciler no-ops on), and shutdown ordering becomes explicit (drain the reconciler before cancelling the pool). `refactor/sink-lifecycle` (`4d24e3f8`) already prototypes this shape for the CLA registry (`drop_tx` + `signal_dropped`); the work is applying the pattern uniformly across `cla::Sink`, `ServiceSink`, and `RoutingSink`. Design rationale in [`streaming_pipeline_design.md`](streaming_pipeline_design.md) §5.1.2; the channel-context alternative (`refactor/sink-to-context`) was considered and rejected — the Sink trait shape stays.

Out of scope here: §5.1.2 also proposes replacing the per-method `Weak::upgrade()` call gating with an `alive: AtomicBool` load. That is an orthogonal hot-path change (call gating, not drop delivery) and should be weighed separately.
