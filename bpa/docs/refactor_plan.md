# Refactor plan — the streaming sequence and the tranches after it

The single working plan for the in-flight refactor effort: the streaming sequence, the filter redesign's remaining Phase 3, and the tranches that follow. Design rationale lives in the design documents ([`filter_subsystem_design.md`](filter_subsystem_design.md), [`streaming_pipeline_design.md`](streaming_pipeline_design.md), [`queue_architecture.md`](queue_architecture.md), [`fragment_reassembly_redesign.md`](fragment_reassembly_redesign.md)) and the two redesign drafts ([`routing_table_redesign.md`](routing_table_redesign.md), [`policy_subsystem_redesign.md`](policy_subsystem_redesign.md)); this is the work list, in dependency order.

## Current state

**Landed:** main carries the v0.3.0 stack — #673 (CLA streaming), #674 (bpv7 parse), #675 (parse), #676 (cbor perf), #677 (metadata) — with #680 (service streaming) and #710 (delivery mirrors forwarding). The #712–#719 train is merged, on bpv7's #753 (the editors refuse what the parser rejects; flags compare by encoding) and #757 (editing and signing keep BPSec operations verifiable): the filter redesign's Phase 2 (#712, which also seats Phase 3's Egress and Deliver hooks), streamed ingress (#717, #718) and streamed origination (#719). The streaming sequence below is under way: egress, delivery, storage and streaming BPSec follow, each its own PR. What else remains: Phase 3's open rows, the ADU reassembly tranche, the per-hop BPSec change, and the queue, policy and routing tranches.

**Streaming seams.** The pull-shaped `Receiver`/`Segment` stream (`bpa::stream`) is the decision of record for all six seams — CLA ingress, service and application originate, both deliveries, and CLA egress — and all six traits take it. A caller holding a whole buffer passes `Bytes`, itself a one-segment `Receiver`; truncation is an error with ack gating; liveness, the size cap and backpressure are composable receiver decorators. A write/commit handle was rejected: parse-at-commit cannot express the pre-drain gate's parse-during-arrival, and push inverts control toward the trivial party. The BPA's own egress and delivery doors hand over the whole bundle as one segment until their legs land.

## Sequencing

1. The streaming sequence (below), one PR per leg so that each review covers one concern: ingress (#717, #718) → origination (#719) → egress → delivery → storage → streaming BPSec. Egress precedes delivery because its payload is plaintext pass-through and needs no streaming cryptography; delivery must decrypt as it streams once the BPSec leg lands, and streams plaintext payloads until then. The storage leg gives `BundleStorage` its streamed write and load and retires `concat_stream`. It precedes streaming BPSec, whose in-flight cryptography reads through it, and it completes the CLA, service and storage surfaces that other work builds on, so the gRPC API (#741) can land while streaming BPSec is under way. Streaming BPSec comes last because it touches both directions.
2. Filter Phase 3's open rows land with the leg or tranche each names.
3. The ADU reassembly tranche is independent of the streaming legs: its `Store` seams carry interim bodies, and its true streaming arrives with the storage leg's streamed load and `replace`.
4. The per-hop BPSec change is its own PR after the train; it needs new `hardy_bpv7` editor operations.
5. Delta fields ride with their consuming tranches: `MetadataDelta` carries the annotation slots (Phase 2) and the `route_table`/`route_key` routing inputs (#718); `class` arrives with the policy tranche.
6. The queue, policy and routing tranches follow the streaming sequence.

Every step ends green: `cargo fmt --check`, `clippy --locked --all-targets --all-features -- -D warnings`, workspace tests, bpv7 `no_std` check.

## The streaming sequence

One PR per leg, in this order, so that each review sees one coherent scope. A leg's PR stays a draft until its planned work is in.

### Ingress — landed (#717, #718)

Bundle reception is one streamed pipeline with one metadata write: the header pass parses and verifies off the CLA stream, the Ingress chain and the route lookup decide at the pre-drain gate, and the payload drains through the validating `ValidatingReceiver`. Bundles are stored as received; the scheduled §5.1.1 removals apply at each output door.

**Gate-then-drain + routing (§5.7 routing-at-commit, #718).** The RIB lookup runs at the pre-drain gate as the decision of record — an explicit Drop rejects without awaiting the payload, and the commit executes the carried decision directly (fresh arrivals skip `DispatchPending`; stale decisions self-correct through the park re-checks against the gate-time snapshot). The drain follows the gate decisions, the ordering a Classifier-set storage QoS (a policy-tranche target) will need: the door decorates the arrival with a `ValidatingReceiver` and drains it through one `Store::save_stream` call only once the chain and the route lookup have settled — a rejected bundle spools nothing and is never persisted, with the door settling the decorator's verdict after the drain and discarding any save the verdict rejects.

**The ingress drain still accumulates in memory before `save`.** `Store::save_stream` (`store.rs`) is the streaming seam at its target signature, but its interim body spools through `concat_stream`, because `BundleStorage` exposes only `save(Bytes)` / `load -> Option<Bytes>`, so there is nowhere to spool a payload without materialising it. The storage leg (below) swaps the backends' streamed write into that body; its callers change only in the error type they map.

### Origination — landed (#719)

Both application doors stream: `ApplicationSink::send_streamed` builds around a segmented payload (`Builder::build_stream`), and the raw door runs the same strict header pass as CLA ingress. Both settle their gate decisions before a payload byte spools, write the record once, and execute the route decision directly. A store-side LRU settles recent duplicates at the input gates.

### Egress — next (#759, `refactor/egress-streaming`)

| Status | Task |
|---|---|
| ✅ | `Peer::forward` composes its own assignment record: the forward path no longer re-derives the peer identity from the channel's target status (on the branch) |
| ✅ | An originated bundle leaves with its built Hop Count: origination is not a hop, so the next node makes the first increment (on the branch) |
| 🔲 | `Store::load_stream`, the egress door's source, at its target signature (a pulled `Receiver<Segment>`): its interim body loads the whole bundle as one segment until the storage leg swaps in the backends' streamed load |
| 🔲 | The scheduled removals, the Egress Rewriters and the per-hop writes (with the legacy re-encode) run on the resident header prefix, and the payload passes through from the streamed load untouched |

### Delivery (#758, `refactor/deliver-streaming`)

| Status | Task |
|---|---|
| ✅ | A delivered payload that fails to decrypt reports why: `FailedSecurityOperation`, or `UnknownSecurityOperation` for an unrecognised context; a missing key parks (on the branch) |
| 🔲 | The scheduled removals and the Deliver chain on the resident header prefix, and streamed delivery of a plaintext payload through both service doors, read through `Store::load_stream` |
| 🔲 | The fixed-vs-pluggable transport-block strip at Deliver (Phase 3 row) |

### Storage — retires `concat_stream`

This is the storage tranche that `Store::save_stream` and `fragment_reassembly_redesign.md` defer to. `BundleStorage` gains its streamed write and load, and `concat_stream`, the BPA's bounded accumulator, goes. By the time it starts, the egress and delivery legs call `Store::load_stream` beside the input doors' `Store::save_stream`, both at their target signatures, so the backends swap in behind them.

| Status | Task |
|---|---|
| 🔲 | The streamed write: `BundleStorage` saves from a pulled `Receiver<Segment>` (`streaming_pipeline_design.md` §3.1) across `bundle_mem`, `localdisk`, `s3` and the `CachedBundleStorage` decorator, swapped into `Store::save_stream`'s body. Nothing is visible under the storage name until the final segment commits, and `max_size` becomes a size-capping receiver decorator |
| 🔲 | The streamed load across the same backends, swapped into `Store::load_stream`'s body. The leg settles the backend shape, which `streaming_pipeline_design.md` §3.1 and §10 Phase B give as a push into a `Sender<Bytes>`, and brings the design doc into line |
| 🔲 | The streamed `replace`, keeping its atomicity (readers see the old bundle or the new, never a partial write). Its consumer is the fragment reassembly redesign's `Store::replace_stream` seam (`fragment_reassembly_redesign.md`, not yet built): if that tranche lands first, this leg swaps the streamed `replace` into the seam's interim `concat_stream` body; otherwise the seam is built streamed |
| 🔲 | `concat_stream` and `ConcatError` removed; the input doors map the size-capping decorator's error in their place. `buffer_stream`, the implementor-side convenience over a declared exact length (the outbound doors, and `ApplicationSink::send_streamed`'s provided body), keeps its own accumulator, and its consumers move to segment-at-a-time as each needs to. The current gRPC clients' CLA `dispatch` and service `send`, whose wire has no streamed message, keep a private bounded accumulator in `proto` until the gRPC API (#741) replaces them; tcpclv4's test sink moves to `buffer_stream` or its own |
| 🔲 | Recovery discards uncommitted spool files (`streaming_pipeline_design.md` §10, Phase A) |
| 🔲 | Settle what `CachedBundleStorage` caches once bundles stream, and whether restart recovery, which loads and parses each stored bundle whole (`restart.rs`), streams too. Durability is a property of a backend's final-segment commit (`streaming_pipeline_design.md` §5.5) and of `recover`, not of the trait, so the streaming BPSec leg's non-durable scratch instance reuses the same trait and backends |
| 🔲 | An async payload verifier, once the stored payload streams: the ingress drain checks the payload's CRC and BIBs, but a BCB-encrypted payload whose ciphertext fails authentication is found only at delivery and occupies storage until then; a background task holding the key could fail it early or stamp "payload verified" in the metadata |

**Revisit `max_bundle_size` after this leg.** The 64 MiB default guards in-memory accumulation and should lift to a much larger (or unlimited-with-opt-in) default once bytes spool; the knob's meaning shifts to custody-admission policy (a half-arrived transfer has no metadata entry, so the reaper and eviction cannot touch it; the spool chokepoint still needs the bound). The lift also needs dynamic storage-headroom admission, and streaming BPSec for encrypted payloads, which delivery decrypts with the whole bundle resident until then.

### Streaming BPSec — the final leg

It touches both directions, so it follows egress and delivery, and its in-flight cryptography reads through the storage leg's streamed load.

| Status | Task |
|---|---|
| 🔲 | Streaming AES-GCM in bpv7 (`bpv7/docs/TODO.md`), so BCB decryption and encryption run as the payload streams. Payload-BIB verification already streams through the ingress drain; BCB decryption still needs the whole bundle resident at delivery |
| 🔲 | A second, non-durable scratch `BundleStorage` in the BPA, which BPSec processing spools a bundle through when an operation needs the tail before it can finish: an authentication tag that must verify before the plaintext is released, or a result (a BIB's MAC, a BCB's tag) that the head carries ahead of the body it covers, rewritten once the body has passed. The scratch need not survive a crash: the stored bundle is as received, so the processing re-runs from it. `streaming_pipeline_design.md` gives the alternatives it is weighed against: decryption that releases plaintext as it streams and withholds only the stream's end until the tag verifies (§5.5), and an egress payload BIB computed in a first pass over the stored payload (§6.1.2) |
| 🔲 | The egress BPSec seat after the per-hop writes (Phase 3 row): the BPA as a security source at egress |

## Per-hop BPSec — its own PR

A per-hop block's security operations end at the next node that changes it, which is their acceptor, so no valid operation puts a per-hop block inside an encrypted BIB. The change makes the forwarder act on that: it disregards or removes an encrypted BIB over a per-hop block, and reports an invalid integrity operation on one. It needs new `hardy_bpv7` editor operations. The rulings and open items are in [`TODO.md`](TODO.md) ("Per-hop blocks under an encrypted BIB park at the next hop" and "Per-hop block handling").

## Filter redesign — Phase 3: hook repositioning + restart re-admission

| Status | Task |
|---|---|
| ✅ | Move the Ingress chain onto the pre-drain gate (#717): a filter Drop follows the gate's reporting pattern (reception + deletion reports per flags; §5.6 report-before-dedup preserved on the early-drop path; arrival-expired stays silent); early Drop skips drain + store. Rode with it: the unified `process_received_bundle` pipeline, the single `insert_metadata` at `Dispatching`, the scheduled §5.1.1 removals (`to_remove`) deferred to the output doors, and the `ValidatingReceiver` drain |
| ✅ | Egress seat in ClaSend: the scheduled §5.1.1 removals (`to_remove`) → registered Rewriters (sequential) → `update_extension_blocks`, the fixed tail of the rewrite stage, whose per-hop writes supersede Rewriter edits to the blocks they write; no Egress Verifier and no Rewriter verdict, so only the legacy re-encode of a primary a BPSec operation covers drops a bundle at Egress; in-memory, per transmission attempt, never written back |
| 🔲 | The BPSec-seam position after the per-hop writes (add BIB/BCB per security policy): designed, not yet built — the BPA signs and encrypts nothing at egress. Lands with the streaming BPSec leg |
| ✅ | Deliver hook before payload decrypt (every local service delivery; edits observable on the raw-`Service` path only): the scheduled §5.1.1 removals, then registered Rewriters (seq: strip transport-scoped extension blocks, reading through the `RewriteContext`'s reader, which decrypts BCB-covered extension blocks and never the payload) then Verifiers ∥; Originate stays pre-store, at the door's gate. `Boundary::Egress { next_hop }` vs `Boundary::Deliver` lets one Rewriter trait serve both boundaries |
| 🔲 | Decide fixed-vs-pluggable strip for the standard RFC transport blocks (Previous Node/Hop Count/Bundle Age) at Deliver. Lands with the delivery leg |
| 🔲 | Define the Originate and Deliver Verifier drop/report semantics (filter doc open item: Originate returns the reason to the service; Deliver drops with reason or deletes), on the §5.6/§5.10 report shape the Ingress gate emits |
| 🔲 | Restart re-admission: bump the Classification group's policy-epoch stamp (present) per restart; lazy re-run at the Dispatch block, chain selected by provenance; classification cleared (`clear_classification`, present, not yet called) and re-derived; the contexts' reader lent over a bounded head read from `BundleStorage` (start → payload data start + P per persisted extents; the storage leg's streamed load with the receiver dropped early — no new storage primitive; no read when no input filters registered); Verifier drops are deletions-in-custody (reports per flags, never a fresh reception report) |
| 🔲 | Wire the declared payload peek at the input gates (Ingress and Originate): hold min(P, payload length) payload bytes on the invocation side of the spool boundary so a peeking Verifier or Classifier reads them. Until then `FilterChains` records P (`max_peek`, frozen at `build()`) with no consumer, and both input chains run on the resident header prefix, where a payload not yet resident reads as `Availability::NotResident` |

## ADU reassembly tranche (`fragment_reassembly_redesign.md`)

Settled, not yet built. Fragments are never bundle records: each pending ADU is one wire-form partial under the reassembled id, with its coverage in `BundleMetadata`, merged door-side under a per-ADU permit, and an Ingress Classifier decides the divert, so the filter chain runs once per ADU. The defects in today's reassembly that it closes are in [`TODO.md`](TODO.md) ("Fragmentation and ADU reassembly").

## Queue tranche — the mechanism layer (`queue_architecture.md`)

The queue architecture replaces the status-based lifecycle with durable queue assignment, and its pull-based `dequeue` deletes the hybrid channel's poller scaffolding (per-cycle channel, spawned forwarding task, cancel-token race). The policy tranche's FlowControllers are the discipline over this mechanism, so this tranche (at least its trait split) lands before the seats bind. Sequencing: after the streaming sequence; this tranche deletes the interim `status` field the metadata partition left on `Bundle`.

| Status | Task |
|---|---|
| 🔲 | `MetadataStorage` split into bundle CRUD (keyed by `Bundle::Id`) + generic queue operations (`enqueue`/`dequeue`/`requeue`/`move_queue`/`drain`, queue id `u32` opaque to storage); `swap_status` maps onto `requeue`, `tombstone_if` onto the conditional move into the Tombstone queue; delete-wins and conditional-move ACID rules per the doc. The split persists `BundleMetadata` plus typed columns in place of the `StoredBundle` JSON blob, deleting two of B8's hot-path re-parses ([`TODO.md`](TODO.md)), and keeps the fragment reassembly redesign's `update_reassembled` exception |
| 🔲 | Backends (mem, sqlite, postgres) to the new trait; the D3 backend-conformance suite ([`TODO.md`](TODO.md)) lands here, pinning `requeue`/tombstone/update-on-deleted semantics across all three, plus the sqlite `StatusFields` port (R3-3) and the sqlite temporal columns as INTEGER epoch milliseconds, in one schema migration |
| 🔲 | Queue schema: `DurableQueue` enum (Dispatch/Waiting/WaitingForService/Fragment/Tombstone) + `QueueFactory` with the durable/ephemeral threshold; the Fragment queue holds the fragment reassembly redesign's pending-ADU records (`ReassemblyPending`). Per-peer egress and transfer-ack parking queues, and per-service delivery and delivery-ack queues, become ephemeral allocations, swept on peer loss, service unregistration and restart to `Waiting` and `WaitingForService` respectively |
| 🔲 | Pull-based `dequeue` replaces the `storage::channel` hybrid poller (plain pull loop; each await a cancellation point); it also moots `metadata_mem`'s `poll_*` scans and clones under its sync mutex ([`TODO.md`](TODO.md) OF-06 records the interim fix) |
| 🔲 | Eliminate the `New` status (never persisted; this row deletes the variant); restart recovery becomes generic ephemeral-queue sweeps (`move_queue` by id predicate: peer queues to `Waiting`, service queues to `WaitingForService`), replacing `restart.rs`'s per-status logic |
| 🔲 | `status` leaves `Bundle` (queue assignment *is* status) behind a `Store`-held status writer whose named transitions move the `bpa.bundle.status` gauge themselves ([`TODO.md`](TODO.md) OF-04, whose `#[must_use]` `Claimed` token is the type-level form of the `OfferOutcome` resolver's claim), retiring the gauge moves outside `Store`; `ForwardPending`'s `next_hop` leaves the status and travels with the forward queue item ([`TODO.md`](TODO.md) OF-03: the `StatusSelector` poll/sweep and the `enqueue_forward` edge); `confirm_exists` becomes `confirm` with this vocabulary |
| 🔲 | [`TODO.md`](TODO.md) items that land in this tranche: P2 (the deferred-Failed pre-swap, subsumed by `requeue`), P3 (the redundant tombstone on Completed, which needs a data-only delete), R3-4 (`PeerId`/`QueueId`/`LaneId` newtypes; the CLA-facing index is a lane, per `policy_subsystem_redesign.md` amendment 5) |
| 🔲 | Waiting sweep reroutes through the dispatch class queues (never bypassing the discipline) — the policy doc's amendment 4, mechanism half here, FlowController half in the policy tranche |
| 🔲 | Apply the five `queue_architecture.md` amendments recorded in `policy_subsystem_redesign.md` when this tranche revises `queue_architecture.md`, adding the per-service delivery queues its ephemeral list and restart sweep omit |

**Shapes the rows keep.** `Bundle` is never deserialized: backends decode a `StoredBundle` that becomes a `Bundle` only through `into_bundle(status)`. The `MetadataStorage` mutation vocabulary is create / status-CAS / terminal (`insert`, `swap_status`, `tombstone_if`/`tombstone`, beside the bulk resets and the recovery protocol), so no write can match a tombstone. Independent of this tranche, and cheapest after its record reshape: seal the (bpv7, extensions) pair into one type only the parse layer can mint. Today the extension cache is set by hand in three `metadata.extensions =` assignments (`ingress.rs`, two in `originate.rs`) and the status-report path's `with_extensions`, and at Egress it holds the values as received while the Rewriters' edits live only in the bytes (`filter/mod.rs`, `forward.rs`).

## Redesign documents

| Status | Task |
|---|---|
| 🔲 | A review pass over the two redesign docs, settling their open questions (`routing_table_redesign.md`: table identity, selection-policy shape, sweep granularity, RoutingAgent API; `policy_subsystem_redesign.md`: `[classes]` format, `ClassId` representation, FlowController trait shape, PolicyAgent protocol, eviction rank, per-source fairness, adaptive de-staging parameters, the storage seat, retry damping, storage QoS as a class property, policy registration). The filter questions are settled in `filter_subsystem_design.md`; its remaining open items are Phase 3 rows |
| 🔲 | Each redesign doc folds into its tracked home (`routing_subsystem_design.md`, `policy_subsystem_design.md`) as its tranche implements it, and the draft retires then |

## Routing and policy tranches (pointers, not tasks here)

- `class` delta field, `ClassPolicy`, the `[classes]` `bpa-server` provider (registered through the public Classifier trait), the redesigned FlowController disciplines (reshaping today's `FlowController`/`FlowControllerFactory`) and the seat bindings → policy tranche (`policy_subsystem_redesign.md`), riding the queue tranche's mechanism; tables, jumps, the FIB compile → routing tranche (`routing_table_redesign.md`), whose key-selection seam — the `route_key`/`route_table` delta fields and the `route_key.unwrap_or(destination)` lookup in the default table — landed with #718.
- Scanner/verdict component and the virtual-CLA re-forward entry point → build when a consumer exists (filter doc Phase 4).

## Landed

- **Filter redesign Phase 2 (#712).** Construction-frozen filter packs replace the runtime filter registry: three synchronous filter kinds (Verifier, Classifier, Rewriter) registered per hook through `FilterPack` and `BpaBuilder::add_filters`, annotation slots and the classification write path, the scoped `ExtensionEditor`, and the engine swap. The built-in validity filters dissolved into the gate and its configuration, and the IPN legacy re-encode is a per-hop built-in (`BpaBuilder::ipn_legacy_peers`). The design is `filter_subsystem_design.md`.
- **Metadata partition (#677).** `BundleMetadata` is partitioned into write-once provenance, the parser-derived extension cache and the classification group, with visibility by field privacy (`filter_subsystem_design.md`, "What lives in bundle metadata").
- **Delivery mirrors forwarding (#710).** Each registered service has a per-service delivery queue (`DeliverPending` → `DeliveryAckPending`) on the forwarding template, and every exit resolves its claim. `queue_architecture.md` ("Current queue types") records the states, `storage_subsystem_design.md` the reaper's hand-off exemption, and `routing_subsystem_design.md` the park re-check.
