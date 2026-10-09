# Fragment Reassembly Redesign (DRAFT / RFC)

> **Status: draft for review.** The ADU-reassembly design. **Nothing here is implemented yet**; the implementation is planned as its own tranche, sequenced after the streamed originate doors. Once implemented, this document folds into the fragmentation sections of [`streaming_pipeline_design.md`](streaming_pipeline_design.md) and [`storage_subsystem_design.md`](storage_subsystem_design.md) and the draft is retired. This document also takes the coherent decision deferred by [`fixing_fragmentation.md`](fixing_fragmentation.md) — see [Resolution of the deferred items](#resolution-of-fixing_fragmentationmds-deferred-items).

## Motivation

Reassembly is the last input-path machinery untouched by the streaming refactors, and every deferred fragmentation finding traces back to its shape. Today each fragment is a full bundle record — parsed, filtered, routed, spooled, and parked as `BundleStatus::AduFragment` — completion is detected by re-polling the whole sibling set on every arrival (`MetadataStorage::poll_adu_fragments`, quadratic over the fragment count), the stitch materialises the entire ADU in RAM (`vec![0; adu_len]`, with no size bound — the ledgered "silent too-large death"), and the tiling check rejects overlapping fragment sets outright, destroying ADUs that RFC 9171 §5.9 says must reassemble.

The rejected alternative is progressive reassembly by random-access writes: each fragment written at its final ADU offset through a `write_at` storage capability, with coverage tracked as an interval set in one metadata row. Random-access writes conflict with the sequential-spool storage model (`streaming_pipeline_design.md` §2.4: bundle data I/O is fully sequential and never modified in place, across all four storage backends), so progressive rewriting here is whole-object, sequential and atomic: each merge writes a new object and retires the old one (decision 5). Three of the alternative's ideas carry over: the `Coverage` interval set (with its test matrix), the single record per ADU, and the dedicated reassembly metrics.

**Strategic context, restated from `fixing_fragmentation.md`:** IETF and CCSDS intend to deprecate RFC 9171 fragmentation in favour of BIBE. This design therefore optimises for correctness and simplicity, not fragment-count throughput, and the whole behaviour sits behind a cargo feature so it can eventually leave the default set.

## Decisions at a glance

1. The divert decision is policy: an Ingress **Classifier sets a `reassemble` flag**; unset defaults to destination locality. The filter chain runs **once per ADU**, at the first fragment only (today it runs per fragment — [`filter_subsystem_design.md`](filter_subsystem_design.md) describes the built seam).
2. Reception status reports fire **per fragment** at the door (RFC 9171 §5.6 reports the bundle received — the fragment).
3. The stored partial is a **full wire-form bundle** — parseable at every step, "best so far" literally: the reaper, restart, and completion all see a real bundle.
4. The merge is **door-driven, one pull chain, store-paced back-pressure**: the fragment's validated stream feeds the store directly through a splice; no staged intermediate blob, no funnel task. Sibling merges serialize under a transient per-ADU permit (a new `hardy_async::sync::AsyncMutex`); a dedicated `storage::channel` per-ADU write seat is ledgered as the possible ultimate shape once the storage tranche lands true streaming backends.
5. The O(N × ADU) whole-object rewrite churn per fragment is accepted, as the price of sequential, whole-object storage I/O. Each merge writes the merged partial as a new object and retires the previous one: nothing is rewritten in place, so storage needs no replace primitive.
6. The `Coverage` ledger lives in **`BundleMetadata`, never in `BundleStatus`** — a growing interval set is too big for a status variant; statuses stay small typed-column values. Disjoint-range DoS cap: **64** ranges.
7. The whole behaviour is gated by a new `hardy-bpa` cargo feature, **`adu_reassembly`**, in the default set today; the data model is deliberately *not* gated (features must not fork the storage schema).

## Architecture

Fragments are never bundle records. There is exactly one record per pending ADU — under the reassembled id (the fragment id minus its fragment fields), status `ReassemblyPending`, born at the first diverted fragment and alive until delivery or expiry.

```text
CLA ingress door (process_received_bundle)
  parse_headers → gates → fragment? ──no──→ existing pipeline unchanged
    │yes
  probe: seen_recently(reassembled id) → settled ADU: report reception, Disposed
         get_metadata(reassembled id):
    ├─ Some(status: ReassemblyPending) → MERGE PATH (no chain, no route):
    │    range already covered / total_adu_length mismatch → drop
    │    reception report, acquire the ADU's merge permit, then ONE PULL
    │    CHAIN into the store: save_stream(new object) ← Splice(headers,
    │    payload ranges from {the raw fragment stream | load_stream(partial)
    │    | ZeroReceiver}, CRC trailer) → update_reassembled(new ledger, new
    │    storage_name) → delete the superseded object → Accepted only after
    │    the commit. No streaming validation on this
    │    path: the fragment's payload CRC and BIB claims are never checked
    │    (see BPSec) — completion validates the whole
    ├─ Some(any other status) → known duplicate: report reception, Disposed
    └─ None → run the chain (Classifier may set the reassemble flag):
         effective flag false → transit fragment: normal pipeline, forwards
           as-is; locally-destined + declined → drop at the gate (a raw
           fragment must never reach a service)
         effective flag true  → FIRST-FRAGMENT PATH: same pull chain with no
           partial source → save_stream mints its storage_name →
           insert_metadata (no-remember variant); an insert refusal
           (tombstone) settles a post-delivery straggler as a duplicate
```

The chain runs once per ADU — at the first fragment — and decides *only* the divert; its other deltas are discarded. The completion run over the reassembled whole is the decision of record, and its deltas persist. Once a partial exists the divert is sticky for every sibling, so a classifier flip mid-ADU cannot split one ADU between paths.

On completion (the arrival whose merge covers the last gap, or the restart arm — see below), the final bytes re-run the ingress machinery inline (`Box::pin` cycle-break, the seat `reassemble.rs` holds today — a whole cannot be a fragment again): `parse_headers` with keys (the payload BIB now verifies over the whole ADU), the gates, the chain, and the route lookup, ending in the completion `update_reassembled` write (`ReassemblyPending` → `Dispatching`) and direct execution of the routing decision. Completion cannot tombstone-and-reinsert — tombstones block same-id re-insert by design — which is why the record flips in place.

### Why the probe order is what it is

- `seen_recently(reassembled id)` is the straggler early-out: a hit means a complete bundle already committed under this id. For that to be true, **the partial's insert must not `remember()` the reassembled id** (a no-LRU insert variant) — otherwise fragment #2 self-poisons the probe. The id enters the LRU only when a complete bundle commits under it: the completion write, a whole-copy insert, or an insert refusal (a refusal also proves a record, so a doomed straggler cycle re-warms the LRU for its successors).
- **Fragment ids never enter the LRU.** A fragment id is `(source, timestamp, {offset, total_adu_length})` — no length — so fragments from different fragmentation episodes can share an id while carrying different byte ranges (the §5.9 overlap case). Id-keyed dedup would discard needed bytes; only ledger containment is a correct fragment dedup.
- A *complete* copy of the ADU arriving through the normal (non-fragment) path while reassembly is pending is a duplicate by id against the partial's record — the existing whole-bundle machinery (LRU or insert refusal) settles it correctly with no special-casing.

### The Classifier contract for `reassemble`

`MetadataDelta.reassemble: Option<bool>` (a new field on the delta, per-field last-writer-wins like `route_table`; surfaced transiently to the door, never persisted). The verdict must derive from sibling-stable inputs — primary-block fields (source, timestamp, destination) — never per-fragment material (offset, frag-0-only extension blocks), because a decline persists nothing and each sibling re-asks. A violating classifier that flips mid-ADU splits the ADU: early siblings forwarded, a late partial stranded to the reaper — bounded and self-cleaning, but wrong; the contract is documented on the field. A declined *locally-destined* fragment drops at the gate (reported, counted): it cannot usefully forward, and delivering a raw fragment to a service is never acceptable. Selective non-reassembly is the feature, not the bug: policy can decline per ADU and let fragments transit.

## The merge funnel

The funnel is the door-driven merge machinery in the new `adu_reassembly` module: a transient per-ADU permit plus one composed pull chain. No task, no channel hop — the CLA session task drives everything, and back-pressure to the CLA is the store's own pull pace by construction: the store's spool pulls the splice, the splice pulls the fragment stream, the CLA blocks on its bounded source.

**The permit registry — transient, merge-scoped.** `hardy_async::sync::Mutex<HashMap<Id, Arc<AsyncMutex<()>>>>` keyed by reassembled id. Per arrival: (1) under the map lock, get-and-clone the `Arc` on hit or insert a fresh entry on miss (the only `Id` clone), map lock released; (2) `permit.lock().await` outside the map lock — a waiting sibling's stream is simply not pulled yet, so the CLA sees pure back-pressure; (3) merge under the guard; (4) guard drops, then GC under the map lock iff `Arc::strong_count == 1` (a racer's clone keeps the entry; a post-GC arrival mints a fresh one — harmless, since only *overlapping* critical sections need shared permit identity). The map is **not** a table of pending reassemblies — pending state is the durable record; an entry exists only while a fragment is physically inside the door, so an ADU idling between contact windows costs zero bytes, and population is bounded by concurrent in-flight fragment arrivals (each of which already owns an ingress spool allowance up to `max_bundle_size`, six orders of magnitude larger than a permit entry). The key must be the `Id`, not a minted context: two racing *first* fragments must collide on the same permit before any record exists to carry a mint. Process-local; restart rebuilds on demand. `DashMap` was considered and declined (std-only in a `no_std` crate, sharding a lock held for two hash ops); a minted-u64 context was considered and declined (cannot cover the creation race, adds restart staleness).

**Prerequisite `hardy-async` work:** `sync::AsyncMutex` — an RAII `lock().await` guard whose `Drop` releases synchronously, so it releases on every exit path including panics; internally the capacity-1-channel token pattern over the existing flume-backed channel (acquire = the queued `send(()).await`, release = sync `try_recv`; no new unsafe, executor-agnostic). A capacity-0 rendezvous cannot bootstrap (the first send would wait for a recv that never comes), hence capacity 1. This also wants the one-line `try_recv` delegation on `hardy_async::channel::Receiver`, which today exposes only async `recv`.

**Per-fragment sequence (merge path, under the permit):**

1. **Re-read under the permit.** The pre-permit `get_metadata` was advisory (it chose the path); re-read after acquiring — a sibling may have updated the ledger, or the reaper may have tombstoned the record while we waited. Tombstoned ⇒ duplicate escape. Range now fully covered ⇒ duplicate escape. `total_adu_length` mismatch ⇒ drop.
2. **Compose the splice**, ordered by output position: (a) header bytes — the assembler's derived form (below); (b) payload `[0, total)` — within the fragment's extent always the fragment's bytes (last-writer-wins on overlap), outside it the partial's bytes where the ledger says covered, `ZeroReceiver` for the gaps; (c) the payload CRC trailer, fed by every payload byte that flowed, then the outer break.
3. **Consume the raw fragment stream — no streaming validation on this path.** A fragment piped to reassembly has neither its payload CRC nor its payload BIBs checked: the splice discards the fragment's own framing and CRC field regardless — the partial's CRC is recomputed over the spliced payload as it flows — and the completion re-run validates the reassembled whole, so per-fragment payload validation buys nothing. The splice takes its payload window from the parsed header extents (definite lengths, known before the drain), forwards only that window into the output, and drains the remainder. The store write is stage-then-commit: a truncated fragment never yields a complete spool, so nothing commits and the transfer is `Refused` (unacked — the peer retransmits).
4. **Ledger write.** `update_reassembled` with the new `ReassemblyState`, the new object's `storage_name` (+ refreshed `extensions` cache when the headers changed); once it commits, the superseded object is deleted. Its CAS on `status == ReassemblyPending` is the reap-race arbiter: a CAS loss means the reaper tombstoned mid-merge — the merge deletes the object it just wrote and settles the fragment as disposed. Only after data commit + ledger write does the door return `Accepted`: a crash earlier was never acked, and the peer's retransmit lands in an idempotent overlay. This is the whole crash story: a crash between the new object's commit and the ledger write leaves the new object unreferenced, and one between the ledger write and the delete leaves the superseded one, and recovery's orphan sweep removes either; there are no fragment blobs to orphan.
5. **Completion check, still under the permit.** Ledger complete ⇒ run the completion inline as described above.

**First-fragment variant:** the same sequence where the under-permit re-read still finds `None` (a racing sibling that won the permit first makes this a normal merge instead), the splice has no partial source, the chain has already run (the divert decision), `save_stream` mints the stable name, and the insert uses the no-`remember()` variant.

## Data model

**`Coverage`**: sorted, disjoint `(start, end)` u64 intervals; binary-search insert merging overlapping and adjacent ranges; complete when one interval spans `[0, total)`. It is hardened against hostile input: a `checked_add` overflow guard on `start + len` (both wire-derived u64s), and a cap of 64 disjoint ranges — an insert that would exceed the cap is refused and the fragment dropped, bounding a hostile every-other-byte fragment pattern.

**`ReassemblyState { total_adu_length: u64, coverage: Coverage }`** as an `Option` group on `BundleMetadata`, serde-elided when `None` so existing records serialize byte-identically (the `RoutingKey`/annotation-slots precedent). Frag-0-seen is derivable — `0 ∈ coverage` iff an offset-0 fragment arrived, since ranges start at fragment offsets. Metadata is the right home beyond size: the frag-0 header swap and the clockless-age edge (below) both require blob writes the status columns could never carry.

**`BundleStatus::ReassemblyPending`** as a bare unit variant. Backends encode it as a plain code (SQLite: next free status code, no params; PostgreSQL: one `ALTER TYPE ... ADD VALUE` migration in its own `-- no-transaction` file, no new column). The unit shape is load-bearing: the reaper's `tombstone_if` status-CAS always matches regardless of intervening ledger merges, so a reap always wins cleanly and the merge's next write fails its CAS and abandons.

**`MetadataStorage::update_reassembled(&Id, &Bundle) -> bool`** — CAS on `status == ReassemblyPending`, atomically rewriting blob + status. This is the **one scoped exception to the metadata-write-once contract**, single-writer (the permit holder), covering both write shapes: a merge (updated `ReassemblyState`, status unchanged) and the completion (state cleared, completion-chain deltas persisted, status → `Dispatching` — durable because restart treats `Dispatching` as chain-complete). Implemented in all three metadata backends. `Store::insert_metadata` gains the no-LRU variant for the partial's birth.

**Store streaming seams**, mirroring `save_stream`'s seam discipline (target streaming signature now, interim resident body, the storage tranche swaps the body with no caller changing): `Store::load_stream(storage_name)` (interim: resident `load_data` — `Bytes` already implements `Receiver<Segment>`) reads the current partial, and the merged partial is written as a new object through `Store::save_stream`, whose storage name `update_reassembled` swaps in under its CAS before the superseded object is deleted. Nothing is rewritten in place, so no replace primitive is needed, and a reader of the superseded object sees it whole until it is deleted. New stream primitives: `ZeroReceiver` and the `Splice` decorator. Memory is bounded by `max_bundle_size` — the same profile as the ingress accumulator — until the storage tranche makes both ends truly streaming.

### The partial assembler

The partial's wire form is rebuilt per merge, and **must not** be built with `Editor::update_block(1)`: that path deliberately strips the edited block from covering BIB/BCB target lists — correct for filter edits, fatal here, since it would destroy the very security blocks completion must verify (a latent flaw in today's stitcher). Instead: the primary derives via `Editor::with_fragment_info(None)` (only the primary is rewritten; every other block, BIBs included, rides verbatim), and the payload block header is hand-emitted exactly as `Builder::build_stream` does — array head, type, number 1, flags, crc_type, the byte-string head declaring `total_adu_length` — with `PayloadTrailer` computing the CRC as the spliced payload flows. A non-zero-offset first fragment yields headers of just the derived primary + payload block (its extension blocks are not authoritative and are discarded); when the offset-0 fragment arrives, its full block set replaces the headers wholesale. Every intermediate is a valid, parseable bundle with correct CRCs — the restart contract.

## BPSec

RFC 9172 §5: security on fragments cannot be evaluated until reassembly. Today a fragment carrying a keyed payload-target BIB hard-fails at every hop — the digest is fed the fragment's partial payload, inline for resident payloads and via the deferred verifiers for streamed ones — which is `fixing_fragmentation.md` item 3. The fix is fragment-awareness in `bpv7`'s keyed verify: when the bundle is a fragment and the target is the payload block, neither verify inline nor begin a (doomed) streaming verifier. The fragment door goes further (ruled): the reassembly path runs **no streaming payload validation at all** — neither the payload BIBs nor the fragment's own payload CRC (its CRC field is discarded by the splice, which recomputes the partial's CRC over the merged payload; a corrupt fragment is caught when the completion re-run validates the reassembled whole). The frag-0 block set (BIBs included) is carried verbatim into the partial's headers, and completion verifies the BIB over the whole reassembled payload — deferral, not exemption. Primary-block-coverage BIBs on fragments keep today's hard-fail: a fragment's primary genuinely differs from the original's, `bpsec/signer.rs` already refuses to sign fragments, and RFC 9172 offers no way to verify them before reassembly anyway. Payload BCBs are unaffected (ingress never decrypts the payload; §5.2's must-replicate flag rule is already enforced at parse).

## Lifecycle and edge cases

**Expiry is the reassembly timeout.** No reaper deferral: `ReassemblyPending` reaps by the partial's own expiry (primary + age — every fragment carries a copy of the original's primary, so the partial's expiry is the ADU's). `received_at`/origin are the first fragment's by construction (the record was born then), preserving the earliest-arrival expiry semantics today's path plumbs explicitly.

**Late fragments after a reap.** Clocked siblings share the original's exact expiry, so they die at the door's existing lifetime gate before the fragment path — silent and free. Clockless (age-based) stragglers re-estimate expiry from their own `received_at` and can be gate-alive after the reap: caught by the LRU while the tombstone is hot, then by the first-fragment insert refusal against the tombstone; once the tombstone itself lapses, a straggler starts a fresh doomed cycle (new partial → expires → re-tombstones) — bounded, self-limiting, accepted (the lever, if ever needed, is tombstone retention, not door logic).

**Clockless-ADU edge:** a non-zero-offset first fragment yields a partial with no Bundle Age block on the wire; the fragment's age value is carried in the record's `extensions.age` metadata cache so the reaper can compute the partial's expiry in that window.

**Restart is reassembly-status-aware.** A dedicated `ReassemblyPending` arm in `restart_bundle` (the partial parses cleanly and `confirm_exists` reconciles against the record's current storage_name, so no data-path special-casing): gauge increment, then — the load-bearing part — if the ledger is complete, **drive the completion path**. This closes the stranding window where the final merge committed (complete ledger, still `ReassemblyPending`) but the crash landed before the completion write: every sibling has already arrived, so no future event would ever re-trigger it. Incomplete ⇒ leave parked; an interrupted merge was never acked, so the peer retransmits.

**The hostile-sibling finding is closed structurally** (the `refactor_plan.md` entry "Offset-keyed fragment set drops the trigger's cleanup on a hostile collision"). The defect — a sibling with the same source/timestamp/offset but different `total_adu_length` evicting the trigger's entry in the offset-keyed `FragmentSet`, leaking its record and gauge — loses its entire surface: fragments are never records, and a mismatched-total sibling drops at the door's consistency check before its stream is pulled, under the same permit as the merge (no TOCTOU; the total is fixed at the first fragment and never mutates). The named, accepted residual: a hostile *first* fragment can poison an ADU with a bogus total, stranding legitimate siblings until the partial expires — the inherent unauthenticated-fragment race (today's stitcher trusts frag-0's total the same way), now bounded by the first-fragment size check (derived headers + `total_adu_length` + trailer against `max_bundle_size`, in u64) and the reaper, with completion-time BPSec as the integrity answer.

**Metrics.** `bpa.bundle.status` gains a `"reassembling"` label; a new `bpa.reassembly.pending` gauge beside the existing `bpa.bundle.reassembled` / `bpa.bundle.reassembly.failed` counters (the latter is described but never emitted today). One record per ADU means today's trigger-vs-sibling gauge asymmetry in the stitcher's cleanup loop disappears.

**Feature gate `adu_reassembly`** (default-on today, so it can leave the defaults when BIBE supersedes fragmentation). Gated: the `adu_reassembly` module (splice assembler, permit registry, `Coverage` use), the ingress fragment divert, the reassembly tests. Deliberately *not* gated — features must not fork the storage schema: `ReassemblyPending`, `ReassemblyState`, `update_reassembled`, and the backend codecs are inert without producers, so a feature-off node still decodes, parks, and reaps records a feature-on replica wrote; `MetadataDelta.reassemble` likewise stays (a no-op flag, additive-features discipline). Feature-off door behaviour: transit fragments forward unchanged; a fragment whose effective disposition is reassemble-here drops at the fragment gate with a reception report and a counted reason.

## What gets deleted

`dispatcher/reassemble.rs`; `storage/adu_reassembly.rs`; `BundleStatus::AduFragment`; `MetadataStorage::poll_adu_fragments` (all three backends); the `Deliver`-fragment arm in `dispatcher/dispatch.rs` (the divert owns locally-destined fragments, so a fragment reaching dispatch simply forwards as routed). The SQLite/PostgreSQL `adu_*` columns stay physically (migrations are append-only); the PostgreSQL `adu_fragment` partial index becomes dead weight and is dropped in a follow-on migration.

## Implementation programme

Its own tranche, sequenced after the streamed originate doors; the usual train discipline (each package staged and gate-verified, committed before the next).

- **P1 — bpv7:** fragment-aware keyed verify (skip payload-target BIBs for fragments in the inline and deferred paths), with tests: a fragment carrying a payload BIB parses and defers nothing; the reassembled whole verifies; a tampered reassembled payload fails.
- **P2 — bpa data model (dormant):** `Coverage` + hardening, ported with its tests from PR #507's `bpa/src/fragmentation/coverage.rs`; `ReassemblyState` on `BundleMetadata`; `ReassemblyPending` unit variant + backend codecs and migrations; `update_reassembled` in all three backends; `status_label` + metrics; reaper/restart arms. Gates green with no producers.
- **P3 — assembler + seams:** `hardy-async` prep (`AsyncMutex`, `channel::Receiver::try_recv`); `ZeroReceiver`, `Splice`, the partial assembler; the `Store::load_stream` shim. Unit tests: byte-identity oracle against a `Builder`-built original; every intermediate re-parses cleanly.
- **P4 — the door + merge + completion; deletions:** `MetadataDelta.reassemble`; the fragment path probe order; the permit registry; the direct-streaming merge; inline completion; delete the old machinery. Feature gate wired; feature-off drop-at-gate behaviour.
- **P5 — docs:** fold into `streaming_pipeline_design.md`, CHANGELOG BREAKING bullets (status enum, storage trait, migrations), close the `refactor_plan.md` reassembly entries (silent too-large death, restart orphan, hostile sibling), ledger the streaming-merge and `storage::channel` follow-ups, retire this draft.

**Verification matrix** (pipeline tests, event-driven per the test style guide): two-fragment happy path; out-of-order and shuffled arrival; overlapping fragments (the §5.9 case today's code destroys); duplicate-fragment idempotence; late fragment post-completion; frag-0-last header replacement; expiry mid-reassembly under a paused clock; restart with an incomplete partial and with a complete-but-undispatched partial; payload-BIB deferral end-to-end with a tampering failure at completion; transit fragments forwarding untouched; classifier-forced and classifier-declined reassembly; the chain running exactly once per reassembled ADU (a counting filter sees one fragment invocation plus the completion run, none on the merge path); mismatched-total hostile sibling; two first fragments of one ADU racing for the permit (the creation race the `Id` key exists for); interleaved merges of independent ADUs. Gates per package include a feature-off clippy/build check (`--all-features` masks feature-off breaks) and a feature-off behaviour test.

## Open questions

1. **Merging across replicas.** The per-ADU permit is process-local, and `update_reassembled` assumes a single writer, the permit holder. Replicas sharing PostgreSQL and S3 are a documented deployment target, and two replicas merging siblings of one ADU would both win the status CAS: one acknowledged fragment is lost and the ADU waits until it expires (nothing is corrupted). Either the CAS also compares the expected `storage_name`, which every merge changes, so it is a version token for free (the loser deletes its object and retries), or multi-replica reassembly is scoped out explicitly. Settle it before P2 fixes the backend signature and migrations.
2. **Overlapping bytes that differ.** RFC 9171 §5.9 defines a fragment's material extents as the bytes that do not overlap any previously received fragment, so the first-received bytes win. This design's merge writes the arriving fragment's bytes over its whole extent (last writer wins), yet drops a fragment whose range is already covered as a duplicate (first wins). The two agree whenever overlapping bytes match, which is every honest sender; which rule applies when they differ should be a deliberate choice.

## Resolution of `fixing_fragmentation.md`'s deferred items

| Item | Resolution |
| ---- | ---------- |
| 1 — §5.9 overlapping fragments | Resolved: the `Coverage` ledger merges overlaps; within a fragment's extent its bytes win (last-writer), outside it the partial's bytes persist. Overlap stops being a failure mode. |
| 2 — deletion reports on reassembly failure | Mostly dissolves: fragments are never held as bundles, so there is nothing to report per-fragment; the failure modes shrink to door-side drops of individual arrivals. The partial itself reports its own deletion when the reaper expires it, flag-gated as any bundle. |
| 3 — fragment payload-BIBs rejected at ingress | Resolved: fragment-aware verify defers payload-BIB verification past reassembly; the completion re-run verifies over the whole ADU. |
| 4 — full-materialisation island | Resolved in shape: one pull chain, no staged intermediate, memory bounded by `max_bundle_size` behind the `load_stream`/`save_stream` seams; the storage tranche swaps the interim resident bodies for true streaming with no caller changes. |
| 5 — quadratic sibling polling | Resolved: `poll_adu_fragments` is deleted; the per-record ledger is the incremental extent tracker item 5 asked for. |
