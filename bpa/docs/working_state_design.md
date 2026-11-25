# Bundle Working State (DRAFT / RFC)

> **Status: draft for review.** Proposes a store for state the BPA accumulates while it processes bundles, kept in the metadata backend beside each bundle's metadata and status. Nothing here is implemented. It gives storage to three tenants: the record of which neighbours hold a copy of a bundle, needed by multi-copy forwarding; the coverage ledger of ADU reassembly, for which it proposes an amendment to [`fragment_reassembly_redesign.md`](fragment_reassembly_redesign.md); and the reassembly of segmented BIBE transfers, which move inside the BPA. Neither multi-copy forwarding nor BIBE in the BPA has a design of its own yet. Once implemented, this document folds into [`storage_subsystem_design.md`](storage_subsystem_design.md) and the draft is retired.

## Motivation

A stored bundle has three parts today. Its bytes live in the bundle store and are never rewritten in place ([`streaming_pipeline_design.md`](streaming_pipeline_design.md) §2.4). Its metadata record holds, by principle, "write-once facts, caches of pure derivations, and BPA infrastructure references — never independent mutable state" ([`filter_subsystem_design.md`](filter_subsystem_design.md), "What lives in bundle metadata"), so that a crash can never tear it. Its status is a small typed value that moves by compare-and-swap as the bundle passes through the pipeline.

None of the three can hold a fact that accumulates while the BPA works and cannot be recomputed from stored bytes. Three features need exactly that:

- **Multi-copy forwarding.** A bundle that is kept after a copy leaves must not be sent to the same neighbour again. Without a durable record of which neighbours hold a copy, every re-dispatch of the kept bundle is a fresh routing decision, and the same neighbour is chosen again on every routing event.
- **ADU reassembly.** The coverage of a pending ADU (which byte ranges have arrived) grows with each fragment, and cannot be read back from the stored partial: a gap is zero-filled, indistinguishable from received zeros. The reassembly redesign puts the coverage in `BundleMetadata` and adds `MetadataStorage::update_reassembled`, which it calls "the one scoped exception to the metadata-write-once contract".
- **Segmented BIBE.** A BIBE transfer arrives as segments, each in an outer bundle of its own, and the inner bundle can be rebuilt only once every segment has arrived. Which ranges have arrived, and where each is held, grows with each segment.

All three are small, durable, mutable records, written as processing advances and useless once their work is done. Two belong to a bundle. The third belongs to a transfer whose bundle does not exist until its last segment arrives. This document gives that kind of state one home and one set of rules, in place of an exception per feature.

## Decisions at a glance

1. **Working state is a fourth part of a stored record**, beside the bytes, the metadata and the status. It holds facts the BPA accumulates while it works that are neither recomputable from stored bytes nor write-once.
2. **The metadata backend stores it**, under rules of its own. No third storage backend is added, and operator configuration does not change.
3. **Every entry is owned by one record and dies with it.** The owner is a bundle record, or a transfer record for a BIBE transfer whose inner bundle does not yet exist. Whatever removes a record removes its working state in the same commit, so nothing outlives its owner and there is nothing to sweep.
4. **Entries are opaque values under a closed set of kinds**, keyed by owner and kind. The BPA encodes each value; backends store bytes.
5. **There is one write primitive, compare-and-swap on the value**, refused unless the owner is a live record. It mirrors `swap_status`.
6. **A write never runs ahead of the fact it records.** Working state may lag the truth, which costs at most a duplicate after a crash; it never claims more than is true. No write has to commit atomically with any other.
7. **Working state is private to the BPA.** Filters never see it, it is never sent on the wire, and it never appears in a status report.
8. **The contract is required of every metadata backend**: a breaking trait change, with no default "unsupported" implementation.

## A bundle's stored record

| Part | Holds | Changes | Seen by filters |
|---|---|---|---|
| Bytes, in the bundle store | the bundle as stored | never in place | through the reader |
| Metadata | provenance, wire caches, classification, the storage reference | written at insert | partly, read-only |
| Status | the bundle's place in the pipeline | compare-and-swap per transition | no |
| Working state | facts accumulated while processing | compare-and-swap per value | no |

All four survive a restart. The BPA's generic machinery, the reaper, the queues and restart's checkpoint resets, never reads working state. Each tenant reads its own kind inside the operation that needs it, and only for an owner whose metadata or status says it has state of that kind, so a bundle with none costs nothing.

## Key design decisions

### Inside the metadata backend, not a third store

Working state is part of the metadata backend's contract, with rules of its own, not a backend of its own. Three properties need it there:

- **Fate-sharing.** Working state describes the records beside it, so an operator who restores a metadata-store backup must get the working state of that moment with it. A separate store restored from a different moment can over-claim: a coverage ledger newer than the record's partial claims bytes that partial lacks, and completes the ADU with zeros where data belongs. [`storage_consistency_updates.md`](storage_consistency_updates.md) puts the node-state ledger inside the metadata store for the same reason.
- **Removal with the owner.** Removing an owner's state in the same commit as the owner is only possible inside one backend. Across two, the removals are separate writes, a crash between them leaves an orphan, and recovery needs a sweep that this design does without.
- **No new configuration.** Operators configure a metadata backend and a bundle-data backend. A third store would add a third choice, and every combination of the three to support.

Rejected alternative: a separate working-state backend. It buys independent placement of a few small values, at the cost of all three properties.

### Owners: bundles, and transfers that have none yet

Working state belongs to a record, so that it has something to die with. For multi-copy forwarding and ADU reassembly the record is a bundle: the bundle being forwarded, and the pending partial, which the reassembly redesign keeps as a record under the reassembled bundle's ID. A BIBE transfer has neither. Its segments arrive in separate outer bundles, and the inner bundle has no ID until the segment carrying its primary block arrives, which may be the last.

A BIBE transfer therefore gets a record of its own in the metadata backend: a transfer record, keyed by the node that encapsulated the inner bundle and the BIBE-PDU's transmission ID, both of which every segment carries. It holds the transfer's working state and an expiry taken from its segments' outer bundles, refers to no stored object, and is removed when the transfer completes or expires.

Rejected alternatives:

- **Anchoring the transfer on its first segment's record.** Every owner would stay a bundle, but a later segment carries only the transfer's key, so finding the anchor needs an index keyed by the transfer: a transfer record by another name.
- **A record under the inner bundle's ID**, as ADU reassembly keeps. The ID is unknown until the segment carrying the inner primary block arrives.

### Opaque values under a closed set of kinds

An entry is keyed by its owner record and a kind, and its value is bytes the BPA encodes. Backends neither parse nor query values, because no tenant reads anything finer than its whole value. Every write replaces the whole value, so each tenant must bound its own, and a state that cannot be bounded is not a tenant.

The set of kinds is closed and owned by `hardy-bpa`. A new kind needs no backend or schema change, only a codec in the BPA and a frozen-encoding test, as the status codecs have. This is not the "stringly" key-value bag that [`storage_consistency_updates.md`](storage_consistency_updates.md) rejects for the node-state ledger: the vocabulary is a closed BPA enum, and an embedder cannot invent a name.

Rejected alternatives:

- **Typed tables per tenant**, such as one row per bundle and neighbour. Appends would need no read, and a backend could index them, but no tenant needs either, and every new tenant would be a schema migration in every backend.
- **Values in the metadata record**, as the reassembly redesign does with coverage. Every tenant would make the record mutable, and the metadata principle exists so that a crash can never tear it.

### One primitive: compare-and-swap

A write replaces an expected value, or its absence, with a new value. It fails if the stored value has changed, or if the owner is not a live record: removed, or never created. A read returns the value or its absence, and a tenant can remove its kind once the state is no longer needed.

The BPA already arbitrates races by compare-and-swap (`swap_status`, `tombstone_if`), and it is enough for every tenant. Writers are rarely concurrent: reassembly serialises them under its per-ADU permit, and the forwarding record is written by the flight that holds the bundle's claim. Rarely is not never, though: a deferred transfer outcome can arrive just as a peer sweep hands the bundle to a new flight, and both have a copy to record. Under compare-and-swap the loser re-reads and writes again, and neither fact is lost; a blind overwrite would drop one. The refusal on a record that is not live stops a late writer from resurrecting state for an owner that is gone.

### A write never runs ahead of its fact

No working-state write commits atomically with anything else: not with a status swap, a record replacement or a bundle-store write. Ordering makes that safe, under one rule: **write a fact only once it is settled, so that no crash can make it untrue; working state may then lag the truth, but never lead it.**

- **The forwarding record is written before the status swap.** A copy has gone to a neighbour once the CLA reports the transfer complete, `Sent` or a deferred `Completed`, and no crash can recall it. The record is written then, before the swap back to `Waiting`. A crash between the two leaves the bundle in `ForwardAckPending`, which restart resets to `Waiting` with the record already correct.
- **The coverage ledger is written after the record replacement.** That a range has arrived is durable only once the merged partial is committed and the record refers to it, so the ledger is written after `update_reassembled`, and the door returns `Accepted` only after the ledger write. A crash between the two leaves the ledger claiming less than the partial holds: the restart arm finds the ADU incomplete, and the peer retransmits the fragment that was never acknowledged. The next merge zero-fills the range the ledger does not claim, so the ledger stays true even when the partial briefly held more.
- **The BIBE coverage is written after its segment is stored.** The [BIBE transfer](#the-bibe-transfer) section shows how.

Lagging costs at most a duplicate after a crash: a copy re-sent to a neighbour whose completion was not yet recorded, a fragment merged twice, or a segment processed twice. The BPA is at-least-once already, since a crash after a transfer and before its tombstone re-sends the bundle, so this adds no new kind of failure. Leading would be worse in every tenant: a forwarding record naming a neighbour that never got the copy, a ledger that completes an ADU with gaps, or a transfer rebuilt from a segment it does not hold.

Removal follows the same rule from the other side: a tenant removes its kind only after the transition that ends the need has committed. Removing the coverage ledger before the completion write, then crashing, would leave a pending ADU that believes it holds nothing while every fragment has already been acknowledged, so none will come again. An entry that outlives its need because of a crash is harmless, and goes with its owner.

Rejected alternative: joint commits, operations that write working state in the same transaction as a status swap or a record replacement. A single backend could provide them, but no tenant needs one for correctness, and each would couple two contracts. One may still be added as an optimisation, if measurement shows the second round trip matters.

### Required of every backend

The operations are required, a breaking change to the metadata backend's trait, implemented by the SQLite, PostgreSQL and in-memory backends. An out-of-tree metadata backend must implement them before it compiles against the new version.

Rejected alternative: default implementations that signal "unsupported", the graceful-degradation contract [`storage_consistency_updates.md`](storage_consistency_updates.md) gives the node-state ledger. That contract fits there because a backend without the ledger loses only an operator override and stays correct. No tenant here has a correct degraded mode: without the forwarding record, a kept bundle goes to the same neighbour again on every routing event, and without their coverage, neither an ADU nor a BIBE transfer can complete.

## Tenants

| | Forwarding record | ADU coverage ledger | BIBE transfer |
|---|---|---|---|
| State | node IDs of the neighbours holding a copy | the coverage set of the pending partial | the coverage of the transfer, and which held segment carries each range |
| Owner | a bundle selected for retention | the `ReassemblyPending` record | the transfer record |
| Written | after each copy is sent, before the status swap | after each merge's `update_reassembled` | after each segment is stored, before its delivery completes |
| Read | inside the route decision for the bundle | at each merge, and by the restart arm | at each segment, and at completion |
| Removed | with the bundle | after the completion write, or with the record at expiry | with the transfer record, at completion or expiry |
| Bounded by | a cap the forwarding design must set | the redesign's cap of 64 disjoint ranges | a cap on segments per transfer, which BIBE's design must set |

### The forwarding record

The record names nodes, not peers. A peer ID is minted for each contact, and does not survive the peer, let alone a restart. A peer may announce several node IDs, so the record takes every node ID the peer announced: a later route that reaches the same neighbour under another name still finds it. A deferred completion that arrives after its peer has gone is ignored today, so its copy goes unrecorded; that is a lag, and costs at most a duplicate.

The record's meaning belongs to the multi-copy forwarding design, not to this document: what counts as a copy, how the route walk excludes a neighbour that holds one, the cap on copies, and when retention ends.

### The ADU coverage ledger

Only the coverage set moves out of the metadata record. The total ADU length is the partial's declared payload length, recoverable from the stored bytes, so it stays in the record as a cache of them. The clockless-age value the redesign keeps in the record's `per_hop` cache stays too: it is a fact taken from a received fragment, not accumulated state. An absent ledger reads as nothing covered, which is the state a crash between the partial's birth and its first ledger write leaves.

With the ledger gone, `update_reassembled` replaces only references and caches: the storage reference, the caches derived from the new bytes, the classification the completion chain derives, and the status. That is consistent with the metadata principle, so the redesign's "one scoped exception" narrows from rewriting independent state to replacing the bytes a record refers to. This amends the redesign's decision 6 and its data model.

### The BIBE transfer

Segmented BIBE moves inside the BPA, so decapsulation and reassembly run there and use working state directly. How the partial bytes are kept belongs to BIBE's own design, but it decides what the working state holds. There are two ways:

- **Held segments (recommended).** Each segment already arrives in a stored bundle, its outer bundle. The BPA holds that bundle until its transfer completes or expires, and the transfer's working state records which held segment carries each range. Completion streams the held payloads in offset order into one new spool, so each byte is read once and written once, with no positional writes and no whole-object rewrites. Held segments are bundle records in a status of their own: they survive restart, and each expires on its own.
- **One merged partial**, as ADU reassembly keeps. A partial inner bundle does not parse as a bundle, and restart deletes any stored object that does not, so this also needs a change to recovery, as well as positional writes or a whole-object rewrite per segment.

Held segments meet the lag rule. A segment's outer bundle is stored at ingress, before decapsulation sees it, so its range is settled before it is recorded. The coverage is written before the segment is held, and a crash between the two re-runs the segment after restart. The repeat finds its range recorded as carried by this same outer bundle, and holds it. Only a different outer bundle carrying a covered range is a duplicate: releasing the very segment the coverage names would make the coverage lead the truth. BIBE's design must keep a segment re-runnable until it is held. Completion follows the removal rule: the inner bundle commits, then the transfer record goes, then the held segments are released, so a crash in between leaves at worst a transfer record or segments that expire on their own.

A transfer expires with its outer bundles. BIBE enforces a sensible lifetime on every outer bundle, so the transfer record needs no lifetime of its own: its expiry is set from the first segment's outer bundle when the record is created, and lowered before any segment whose outer bundle expires sooner is named in the coverage. The earliest expiry is the right one, for two reasons. A transfer cannot complete once any of its segments has gone. And its coverage must never name a segment that has gone, which holds as long as the transfer expires first. A crash between lowering the expiry and naming the segment can only make the transfer expire early, and at equal expiry the reaper removes the transfer before the segment.

Rejected alternative: dropping only an expired segment's range and letting the transfer continue. It would let the transfer complete if that segment were sent again, but it makes the reaper a writer of working state.

## Integration

- **`MetadataStorage`** gains the working-state operations (read, compare-and-swap and remove) and transfer records (create, read, lower the expiry, and remove). Lowering an expiry keeps the earlier of the stored and the new value, so concurrent segments of one transfer need no compare-and-swap for it. Whatever removes a record, bundle or transfer, removes its working state in the same commit: `tombstone`, `tombstone_if`, transfer removal, and the recovery removal of unconfirmed records. PostgreSQL removes a bundle record by deleting its metadata row, so a foreign-key cascade does it. SQLite tombstones by clearing the row instead of deleting it, so a cascade never fires; a trigger on the cleared row removes the working state, so no tombstoning statement can forget to. The in-memory backend drops a record's entries wherever it drops the record, eviction included. Recovery must leave transfer records alone: they refer to no stored object, so the unconfirmed-record sweep has nothing to confirm them against.
- **`Store`** gains thin accessors over the operations, and owns the per-kind codecs.
- **The reaper** expires transfer records beside bundles, in expiry order, removing a transfer before a segment that expires at the same moment; that is its one addition. Restart gains no phase and no sweep, because an owner's removal removes its state. The reassembly restart arm reads its coverage from working state instead of the record.
- **Document amendments carried by this design:** the reassembly redesign's decision 6, data model and `update_reassembled` wording (coverage moves to working state, and the total length stays as a cache of the bytes); [`storage_subsystem_design.md`](storage_subsystem_design.md)'s dual storage model (the metadata backend holds working state and transfer records under a second contract); and [`filter_subsystem_design.md`](filter_subsystem_design.md)'s "What lives in bundle metadata" (independent mutable state has a home outside the metadata).
- **Changelogs:** breaking entries for `hardy-bpa` (the trait), and for `hardy-sqlite-storage` and `hardy-postgres-storage` (schema migrations).

## Standards compliance

Working state is never a retention constraint, which RFC 9171 §3.1 defines as "an element of the state of a bundle that prevents the bundle from being discarded": it keeps no bundle alive, and dies with its owner. The forwarding record serves forwarding to "some other node(s)" (RFC 9171 §5.4). The coverage ledger serves RFC 9171 §5.9 reassembly, and its handling of overlapping fragments does not change. BIBE transfers are keyed by the BIBE-PDU's transmission ID, whose use [`bibe/docs/design.md`](../../bibe/docs/design.md) describes.

## Implementation programme

- **W1, the contract (dormant):** the trait operations, transfer records, the three backends and their schema migrations, the `Store` accessors and the kind codecs. Transfer records land here, with no producer yet, so that the trait breaks once. Gates green with no tenant.
- **W2, the coverage ledger:** lands with the reassembly tranche, in place of the redesign's metadata field.
- **W3, the forwarding record:** lands with multi-copy forwarding, once that design is settled.
- **W4, BIBE transfers:** land with BIBE's move into the BPA, once that design is settled.

## Open questions

1. **The name.** "Working state" is proposed over "scratch store": "scratch" suggests state that need not survive a crash, and [`refactor_plan.md`](refactor_plan.md) already uses it for a non-durable `BundleStorage` that BPSec processing would spool through.
2. **Trait shape.** Whether the operations are methods on `MetadataStorage` or a trait of their own that every metadata backend must also implement, and how a kind and an owner (a bundle ID or a transfer key) are represented at the trait surface.
3. **Filter-owned working state.** A filter pair may want durable per-bundle state that changes as the bundle is processed, such as Spray-and-Wait's count of remaining copies, which a slot cannot hold: a slot is "never a ledger". The closed set of kinds keeps such state out until a design admits it, including how an egress Rewriter, which runs once per transmission attempt, would write durable state.
4. **Positional writes in the bundle store.** ADU reassembly would shed its whole-object rewrites with them; held segments give BIBE none to shed. They change the sequential-spool rule, and need their own design.
5. **Joint commits.** Whether to add, as an optimisation, an operation that writes working state in the same transaction as a status swap.

## Testing

- [Storage integration test plan](../../tests/storage/docs/test_plan.md): gains a working-state suite covering compare-and-swap, refusal for an owner that is not live, removal by every path that removes a record, bundle or transfer (the recovery removal included, which must leave transfer records alone), and the frozen encoding of each kind.
