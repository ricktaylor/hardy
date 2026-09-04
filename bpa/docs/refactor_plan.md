# Refactor plan — v0.3.0 stack and the filter redesign

The single working plan for the in-flight refactor effort: finishing the v0.3.0 stack, then implementing the design in [`filter_subsystem_redesign.md`](filter_subsystem_redesign.md) (companions: [`routing_table_redesign.md`](routing_table_redesign.md), [`policy_subsystem_redesign.md`](policy_subsystem_redesign.md)). Design rationale lives in those documents; this is the work list, in dependency order.

## Current state

**Stack:** `main (a7be0d5c, post config-ownership #664) → refactor/cla-streaming (99b9e674, +4) → refactor/bpv7-parse (8e20c387, +9) → refactor/parse (5a3b364b, +24) → refactor/cbor-perf (3ef79e32, +4) → refactor/metadata (c42515a3, +7)`, fully linear, signed, pushed 2026-08-17. The chain was rebased over everything main merged through #664 on 2026-08-14 and verified green in-container: fmt, clippy `--all-targets --all-features` zero warnings, workspace tests, bpv7 `no_std` check, plus `--locked` workspace builds of all five tips.

**Streaming seams (2026-08-17):** `refactor/cla-streaming` (#673) and `refactor/service-streaming` (#680) are review-complete (round 3: land-ready) — `Segment`/`Receiver` + capped `concat_stream` in `bpa::stream`, `dispatch_streamed`/`send_streamed` as required primitives with provided whole-buffer conveniences, truncation-as-error with ack gating, per-segment sink-side liveness, `max-bundle-size` config. **Decision of record: the pull-shaped `Receiver`/`Segment` stream is the target for all six seams** (CLA ingress and service originate landed; application originate, both deliveries, and CLA egress — the last pull-shaped in the reverse direction — to come). The reviewer's write/commit handle was rejected: parse-at-commit cannot express the pre-drain gate's parse-during-arrival, push inverts control toward the trivial party, and liveness/cap/backpressure are composable receiver decorators (full riposte in `references/reviews/cla-streaming.md`). When the legs merge: cascade the draft PRs over them asserting the guards in `TODO.md` (gate drain inherits `max_bundle_size`; sink-side liveness wrappers survive; every `ServiceSink` carries `send_streamed`).

## Sequencing

1. ~~Extraction PRs and the cla-transfer-outcome chain merge~~ — done, all in main by #664.
2. ~~Stack re-cut~~ — done (see below); the Commit 1 metadata partition rode it as planned.
3. Filter Phases 2–3 target the current stack.
4. Delta fields ride with their consuming tranches: `MetadataDelta` ships **empty** in Phase 2; `class`/`route_key` arrive with the queue/policy and routing tranches respectively.

Every step ends green: `cargo fmt --check`, `clippy --locked --all-targets --all-features -- -D warnings`, workspace tests, bpv7 `no_std` check.

## Stack re-cut (done 2026-08-14)

The chain is cut into the 5 PR-shaped branches above: cla-streaming → `refactor/bpv7-parse` (parser split + BPSec mechanism + PICS — the bpv7 API story) → `refactor/parse` (streaming-ingress remainder incl. the gate/§4.3.2 commits) → `refactor/cbor-perf` → metadata (restructure + style only). Of the planned fix-pass items: main's post-fork tests (metadata_mem/sqlite CAS suites, restart recovery) are adapted at their owning commits; the rustfmt diffs are folded in; all five tips verified green in-container. The known non-compiling window mid-`refactor/parse` (tuple-threading, restored by the canon-restore commit) remains — deliberate, matches the pushed history.

**Standing rebase guard** (see also `TODO.md`): every replay onto a newer main must re-assert the CAS conditional moves (channel `send` swap, dispatch Waiting fallbacks, `forward_bundle`'s ForwardAckPending claim + `pre_rewrite` restore), confirm every `Sink` impl carries `dispatch_streamed`, and confirm tcpclv4's `max_outstanding_transfers` builder knob survives. Run clippy with `--all-features` — it is what compiles the `cfg_attr` instrument attrs.

## Filter redesign — Phase 1: documents (after the review pass)

| Status | Task |
|---|---|
| 🔲 | Rick's review pass over the three redesign docs; settle the open questions (filter: exposed `&Bundle` view, non-Ingress drop reporting, Rewriter naming + handle ops; routing: table identity, sweep granularity, RoutingAgent API; policy: `[classes]` format, `ClassId` representation, `FlowController` trait shape, PolicyAgent protocol, eviction admission test, per-source fairness) |
| 🔲 | Fold accepted drafts into `filter_subsystem_design.md` / `routing_subsystem_design.md` / `policy_subsystem_design.md`; retire the drafts; apply the four `queue_architecture.md` amendments (recorded in the policy doc); back-reference from `streaming_pipeline_design.md` §5.3 |

## Filter redesign — Commit 1: metadata partition (landed on `refactor/metadata`)

Target shape per the filter doc's "Sketch — the concrete types" section: visibility by field/module privacy on the record itself (no view structs; block-body access via the existing bpv7 `payload`/`extract` accessors); the commit takes the record shape only — queue items and `MetadataDelta`/slots are Phase 2/queue-tranche material.

| Status | Task |
|---|---|
| ✅ | `Provenance { received_at, origin }` with `origin: Ingress { cla, peer_node, peer_addr } \| Originated \| Recovered` — **persisted, write-once** = private fields + `pub` read accessors + no `&mut` accessor. `Recovered` covers restart orphans (data without a metadata record), where fabricating an Ingress origin would be the lie the partition exists to kill |
| ✅ | `ExtensionFields { previous_node, age, hop_count }` — parser-derived cache of the stored bytes; plain `pub` fields (filters only ever hold `&Bundle`) |
| ✅ | `Classification` — empty placeholder group, serde-persisted, **private field** (getters/`apply()` arrive with Phase 2); fields arrive with their tranches |
| ✅ | `storage_name` stays a `pub(crate)` field — already unreachable outside the crate; a one-field `Infrastructure` struct added ceremony without enforcement |
| ✅ | Constructors `BundleMetadata::ingress(…)` / `::originated()` / `::new(received_at, origin)` (the explicit-parts primitive for record reconstruction — storage backends, fixtures, recovery paths); `Default` removed |
| ✅ | `next_hop` → transient `#[serde(skip)]` **`pub`** field on `BundleMetadata` — `ipn-legacy-filter` reads it, so it narrows to invocation context only when Phase 2/3 delivers the Egress seat (the sketch's `ForwardItem`) |
| ✅ | `status` and `writable: WritableMetadata` stay in place, interim (queue tranche and Phase 2 respectively delete them); reassembled ADUs carry the earliest-arriving fragment's origin alongside its `received_at` |
| ✅ | Mechanical sweep done (24 files, +419/−481): `read_only.*` → accessors/`wire.*`; `core.rs` helpers re-expressed over the groups; `metadata_mem`/storage/fixtures tests adapted; BREAKING serde-shape entry in the changelog. Signed and pushed with the 2026-08-17 stack |

## Filter redesign — Phase 2: traits + engine swap (incremental-safe)

Lands as `refactor/filters`, a new leg atop `refactor/metadata`. Every commit ends green per the standing gate line; hook **positions are unchanged** throughout (ingress still post-store — repositioning is Phase 3).

**Decisions (settled 2026-08-17):** ipn-legacy's primary-block rewrite becomes a config-driven fixed built-in in the ClaSend rewrite stage (not a Rewriter — the primary block is out of scope by design); a Classifier sees the deltas applied by preceding links of the same pass (engine applies each delta before the next invocation); filter invocations **keep key access** (a `KeySource` argument, resolved through the existing KeyProvider seam — committed API); Phase 2 keeps today's per-hook drop behaviour verbatim, formal Originate/Egress/Deliver verdict semantics defer to Phase 3; the kind is named **Rewriter**; the scoped editor **refuses** edits to blocks under existing BPSec coverage.

| Status | Commit | Task |
|---|---|---|
| 🔲 | C1 | Classification write path + annotation slots: `Classification` gains `slots: SlotMap` + `epoch: PolicyEpoch` (no accessor) with `apply(delta)`/`clear_classification(epoch)` as the only write paths; `MetadataDelta` (`#[non_exhaustive]` + `Default`, **slots only** — `class`/`route_key` ride their tranches); `SlotHandle<T>` from Builder registration only; serde-at-rest slot values, per-slot size bound, name collision = `build()` error, unknown-name-on-load dropped. Pure addition, nothing consumes it yet |
| 🔲 | C2 | `Verifier` + `Classifier` + `Rewriter` traits (each invocation: `&Bundle`, `data: &[u8]`, `&dyn KeySource`; Egress additionally next-hop context); one verdict enum; the scoped block-editor handle (insert/replace/remove extension blocks only — no primary, no payload, no BIB/BCB; refuses BPSec-covered targets); `Builder::add_*` per hook and kind with the payload-peek prefix argument (default 0); **chain order = call order**; `build()` computes P = max declared, freezes plain slices; names survive only as diagnostic labels. Old machinery still drives execution |
| 🔲 | C3 | Engine swap: the four dispatcher `exec` sites run the frozen chains (Verifiers ∥ then Classifiers seq at inputs; Rewriters seq then Verifiers ∥ at Egress). Delete `ReadFilter`/`WriteFilter`, `WriteResult`, `Mutation`, `ExecResult`, the `chain.rs` re-validation + payload plumbing, the `FilterEngine` registry (names, dependency graph, `Error::{AlreadyExists, DependencyNotFound, HasDependants}`), `Bpa::{register,unregister}_filter`, `Builder::filter`; `WritableMetadata` deleted with `flow_label`. The runner returns the bundle alongside any error — retiring `forward_bundle`'s `bundle_id` clone + re-fetch restore path (TODO S-7) and keeping the ForwardAckPending claim resolution CAS-clean. Migrate consumers: bpa-server's two registration sites; pipeline tests (INT-BPA-06's ExtentCheckFilter → Egress Verifier — must survive, it guards the extent-consistency fix); ipn-legacy-filter → the new ClaSend built-in (peer-pattern list moves to bpa `Config`; the crate retires or thins to config types) |
| 🔲 | C4 | Dissolve the built-ins: delete `BundleValidityFilter` (pre-drain gate covers ingress; `originate_bundle` gains the inline lifetime/hop check for the raw path); `Rfc9171ValidityFilter` → checks/gate layer driven by the two `Config` booleans (`primary-block-integrity`, `bundle-age-required`, strict defaults — main's config-ownership rework already flattened the filter to fluent setters, halfway there); remove the `no-rfc9171-autoregister` cfg feature |
| 🔲 | C5 | CHANGELOG (BREAKING: registration surface — `register_filter` dies, `Builder::add_*` is born) and fold the settled shapes back into the design doc |

## Filter redesign — Phase 3: hook repositioning + restart re-admission

| Status | Task |
|---|---|
| 🔲 | Move the Ingress chain onto the pre-drain gate: a filter Drop follows the gate's reporting pattern (reception + deletion reports per flags; §5.6 report-before-dedup preserved on the early-drop path; arrival-expired stays silent); early Drop now skips drain + store |
| 🔲 | Egress seat in ClaSend: `update_extension_blocks` becomes the fixed head of the rewrite stage → registered Rewriters (sequential) → Egress Verifiers (parallel) → the BPSec-seam position; in-memory, per transmission attempt, never written back |
| 🔲 | Deliver hook before payload decrypt; Originate unchanged (pre-store, in-memory) |
| 🔲 | Define the non-Ingress Verifier drop/report semantics (filter doc open question: Originate returns an error to the service; Egress/Deliver drop-with-reason vs delete) |
| 🔲 | Restart re-admission: policy-epoch stamp in the Classification group; lazy re-run at the Dispatch block, chain selected by provenance; classification cleared and re-derived; invocation `data` supplied by a bounded head read from BundleStorage (start → payload data start + P per persisted extents; implemented as plain `load` with the receiver dropped early — no new storage primitive; no read when no input filters registered); Verifier drops are deletions-in-custody (reports per flags, never a fresh reception report) |

## Remaining streaming work (descendant branch)

**`BundleStorage::save_stream` is the only remaining prerequisite for true streaming.** When it lands, revisit `max_bundle_size`: the 64 MiB default guards in-memory accumulation and should lift to a much larger (or unlimited-with-opt-in) default once bytes spool — the knob's meaning shifts to custody-admission policy (a half-arrived transfer has no metadata entry, so the reaper and eviction cannot touch it; the spool chokepoint still needs the bound). True retirement waits on chunk-capable BPSec (verify/decrypt currently need the whole bundle resident) plus dynamic storage-headroom admission. The ingress drain still accumulates the payload tail in memory before `save`; `BundleStorage` exposes only `save(Bytes)` / `load -> Option<Bytes>`, so there is nowhere to spool a payload tail without materialising it. Add `save_stream` (a push-side `StreamIn<Bytes>` mirroring `recover`'s `Sender` pattern) across `bundle_mem`, `localdisk`, and `sqlite`, plus the `store.rs` wrapper, then swap the in-memory accumulator in `drain_payload` for it.

**Async payload verifier** (deferred until load-side streaming exists): a corrupted payload is not detected until delivery and occupies storage until then; a background task streaming the stored payload could fail it early or stamp "payload verified" in the metadata.

## Queue tranche — the mechanism layer (`queue_architecture.md`)

The queue architecture is a load-bearing streaming component: it replaces the status-based lifecycle with durable queue assignment, and its pull-based `dequeue` deletes the hybrid channel's poller scaffolding (per-cycle channel, spawned forwarding task, cancel-token race) that the streaming pipeline currently threads through. The policy tranche's FlowControllers are the discipline over this mechanism — this tranche must land (or at least its trait split) before the seats bind. Sequencing: after the v0.3.0 stack (the metadata partition's interim `status` field is what this tranche deletes), interleaving with filter Phase 2 (independent surfaces).

| Status | Task |
|---|---|
| 🔲 | `MetadataStorage` split into bundle CRUD (keyed by `Bundle::Id`) + generic queue operations (`enqueue`/`dequeue`/`requeue`/`move_queue`/`drain`, queue id `u32` opaque to storage); `swap_status` maps onto `requeue`, `tombstone_if` onto the conditional move into the Tombstone queue; delete-wins and conditional-move ACID rules per the doc |
| 🔲 | Backends (mem, sqlite, postgres) to the new trait; the D3 backend-conformance suite (TODO.md) lands here, pinning `requeue`/tombstone/update-on-deleted semantics across all three — plus D4 (`replace` under the delete-is-terminal doctrine) and the sqlite `StatusFields` port (R3-3) |
| 🔲 | Queue schema: `DurableQueue` enum (Dispatch/Waiting/WaitingForService/Fragment/Tombstone) + `QueueFactory` with the durable/ephemeral threshold; per-peer egress and transfer-ack parking queues become ephemeral allocations, swept to Waiting on peer loss and restart |
| 🔲 | Pull-based `dequeue` replaces the `storage::channel` hybrid poller (plain pull loop; each await a cancellation point) — the scaffolding the queue doc deliberately left in place |
| 🔲 | Eliminate `New` status (stored-but-unqueued = mid-ingestion); restart recovery becomes the generic ephemeral-queue sweep (`move_queue` by id predicate), replacing `restart.rs` per-status logic |
| 🔲 | `status` leaves `BundleMetadata` (queue assignment *is* status — completes the partition commit's declared interim); `next_hop` moves into `ForwardItem` per the filter doc's queue-item sketch (`DispatchItem`/`ForwardItem` own `Bundle` by value) |
| 🔲 | TODO.md items that land with `requeue`: P1 (ingress double status write — the send's CAS becomes the checkpoint), P2 (deferred-Failed pre-swap subsumed), P3 (redundant tombstone on Completed), R3-5 (claim RAII guard), R3-4 (`PeerId`/`QueueId`/`LaneId` newtypes — the CLA-facing index is a *lane*, per the policy doc's queue/lane vocabulary — sequenced here where queue ids become API) |
| 🔲 | Waiting sweep reroutes through the dispatch class queues (never bypassing the discipline) — the policy doc's amendment 4, mechanism half here, FlowController half in the policy tranche |
| 🔲 | Apply the four `queue_architecture.md` amendments (recorded in `policy_subsystem_redesign.md`) when revising the tracked doc — with the doc-fold rule: redesign docs stay separate until the streaming rework completes |

## Queued behind other tranches (pointers, not tasks here)

- `class` + `route_key` delta fields, `ClassPolicy`, the `[classes]` `bpa-server` provider (registered through the public Classifier trait), FlowControllers and the seat bindings → policy tranche (`policy_subsystem_redesign.md`), riding the queue tranche's mechanism; `route_key.unwrap_or(destination)`, tables, jumps, the FIB compile → routing tranche (`routing_table_redesign.md`).
- Scanner/verdict component and the virtual-CLA re-forward entry point → build when a consumer exists (filter doc Phase 4).

## Reference — gotchas

- Container has no `ssh-keygen`; repo signs commits (`commit.gpgsign=true`, `gpg.format=ssh`).
- A rebase dissolves duplicated cherry-picked commits that a 3-way merge cannot — prefer rebase for these stacks.
