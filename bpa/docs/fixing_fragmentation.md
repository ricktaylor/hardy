# Fixing fragmentation

Fragmentation and ADU reassembly work today, but an examination of the reassembly path against the streaming pipeline model (2026-07-16, on the v0.3.0 refactor stack) found conformance gaps, a memory-model hole, and several smaller items. None of them block the streaming work; several of them are best fixed *with* it. This document gathers everything fragmentation-shaped in one place — including items harvested from `TODO.md` — so a coherent decision about fragmentation can be taken once the bulk of the streaming work lands, rather than patching piecemeal.

**Strategic context:** IETF and CCSDS intend to deprecate RFC 9171 ADU fragmentation once BIBE standardises as its replacement (segmented encapsulation — BPSec-clean where fragmentation is not: RFC 9172 §5 forbids adding security blocks to fragments). The machinery below therefore stays correct for RFC 9171 conformance and interop with existing implementations, but the coherent decision this document defers should weigh every item against that horizon: prefer the minimal fix — or an explicit wontfix — for anything BIBE-based segmentation obsoletes.

## What holds today (verified)

The re-entry design is sound and worth keeping: a reassembled ADU re-runs the full ingress pipeline (`dispatcher/reassemble.rs` → `process_received_bundle`) — re-parse, keyed BPSec, expiry/hop gate, dedup, store — so ingress semantics are inherited by construction, and BPSec over the reassembled whole verifies correctly without any special-casing. Item 6 records the half of that re-run reception should not repeat.

Expiry accounting composes with the expired-at-ingress silence policy: buffered fragments (`BundleStatus::AduFragment`) are reaper-watched, so in-custody fragment expiry produces RFC 9171 §5.10 deletion reports, while the per-fragment ingress gate remains a true arrival refusal. Consumed fragments are tombstoned (replays refuse as duplicates), the reassembled ADU dedups under its defragmented id, and the tiling arithmetic is u64-first and 32-bit safe.

The reassembled bundle now carries the earliest `received_at` across its fragment set (plumbed from `FragmentSet` through `ReassemblyResult::Done`), so the ingress gate's expiry estimate for no-clock sources is computed from first arrival, not last.

On expiry inputs generally: every fragment's primary block is a copy of the original's (RFC 9171 §5.8), so the ADU's creation timestamp and lifetime are already in hand at every fragment arrival and again at re-entry — no extra parsing is needed, and for clocked sources the gate's expiry decision is exact throughout. The `received_at` plumbing matters only for no-clock sources (creation timestamp 0), where expiry must be anchored to arrival time minus Bundle Age. One residual imperfection remains there: the rebuilt bundle carries fragment 0's Age block, while the plumbed `received_at` is the earliest arrival — which may be a different fragment, so the anchor pairs one fragment's arrival with another's age. If exactness ever matters, the fix is to track `min(received_at − age)` per fragment set instead of `min(received_at)`; given it only affects no-clock sources with multi-path fragment skew, it is noted here rather than done.

## 1. RFC 9171 §5.9 material-extents reassembly (overlapping fragments)

*Harvested from `TODO.md`.*

### Background

`Store::reassemble()` (`src/storage/adu_reassembly.rs`) requires the received fragments to tile `[0, total_adu_length)` exactly: contiguous, non-overlapping and complete. Overlapping fragment sets are rejected and the fragments dropped as `ReassemblyResult::Failed`.

RFC 9171 §5.9 is more permissive: overlapping fragments are legal on the wire (e.g. the same bundle refragmented differently on divergent paths), and a conformant reassembler computes each arriving fragment's "material extents" — the byte ranges not already covered by previously received fragments — completing when the material extents concatenate to the full ADU. First-received bytes win; overlap is trimmed, not rejected.

Hardy has never accepted overlap: the pre-tiling-check code also failed overlapping sets (payload-length sum ≠ total), except for the length-sum coincidence that silently delivered a corrupt ADU (2026-07-08 review findings #1/#4). The tiling check makes rejection deterministic and safe, but Hardy remains non-conformant for legitimately overlapping fragment sets.

The 2026-07-16 examination sharpens the severity: because the `Failed` path deletes **and tombstones** every sibling fragment, a legitimate overlapping retransmission does not merely fail once — it permanently destroys an ADU whose complete coverage existed (or was still arriving), and the tombstones then refuse the retransmitted fragments as duplicates. See item 2 for the deletion-report half of that path.

### What full §5.9 support needs

- `FragmentSet` must hold *trimmed* ranges decided at insert time by arrival order: on insert, clip the new fragment's payload range against the extents already covered (possibly splitting it), rather than keying whole fragments by raw offset.
- The completeness gate in `poll_fragments()` (`adu_totals >= total_adu_len`) must sum material extents, not raw payload lengths, or completion fires early on overlapping sets.
- The copy loop in `reassemble()` then slices each stored payload sub-range; the tiling invariant holds by construction.
- §5.9 requires the reassembled ADU to replace the payload of the fragment whose material extents include offset zero — the current "fragment 0" special-casing needs re-deriving from material extents, not from a raw offset-0 key.

## 2. Deletion status reports on reassembly failure

*Harvested from `TODO.md`.*

When reassembly fails (`ReassemblyResult::Failed`), `Store::adu_reassemble` deletes the held fragments directly against storage (`delete_data` + `tombstone_metadata`) and the dispatcher's `Failed` arm returns without action — no deletion status reports are generated. RFC 9171 §5.10 says a deletion status report SHOULD be generated per deleted bundle (each fragment is its own bundle, reported to its own report-to EID with its fragment offset/length) when the report flag is set and reporting is enabled.

The fix is plumbing, not policy: `adu_reassemble` should hand the fragment `Bundle`s back on failure instead of consuming them, so `Dispatcher::reassemble` can route each through `drop_bundle(bundle, reason)` (which already does the flag-gated `report_bundle_deletion` + delete). Reason-code selection per failure mode needs deciding: `DepletedStorage` fits the length-not-addressable case; coverage gaps/overlaps have no exact RFC 9171 reason code (`NoAdditionalInformation` or `BlockUnintelligible` are the candidates).

Note the interaction with item 1: once material extents are implemented, overlap stops being a failure mode at all, and `Failed` shrinks to genuinely corrupt/misaligned sets — which makes report-on-failure both rarer and more clearly correct.

## 3. Fragment-carried payload BIBs are rejected at fragment ingress (RFC 9172 §5.2)

RFC 9172 §5.2 forbids *adding* security blocks to a fragment ("a BCB or BIB MUST NOT be added to a bundle if the 'Bundle is a fragment' flag is set") — Hardy's `Signer`/`Encryptor` enforce that. But a bundle signed *before* fragmentation legitimately yields fragments carrying a payload-targeting BIB (it rides in the first fragment as an ordinary extension block), and that signature spans the whole original payload, so it **cannot be verified until reassembly**. §5.2's BCB clause ("BCBs MUST have the 'Block must be replicated in every fragment' flag set if one of the targets is the payload block") explicitly anticipates security-then-fragmentation traffic.

Hardy's keyed ingress has no fragment exemption: payload-target BIBs verify over the fragment's partial payload — inline at the header parse when the payload is resident, through the deferred verifiers the ingress drain feeds when it streams — so verification fails, and the fragment is dropped at ingress. Signed-then-fragmented traffic from other implementations (ION fragments readily) therefore never reaches reassembly. This is shared with main — the streaming refactor did not cause it — but the streaming refactor built the natural fix point: the `deferred_bibs` machinery should defer payload-BIB verification *past reassembly* when `primary.flags.is_fragment` is set, since the re-entry pipeline already verifies the reassembled whole correctly. Payload BCBs are unaffected: ingress does not decrypt the payload, so encrypted fragments already pass.

**Resolved.** Ingress checks nothing of a fragment's own: `Parsed::check` defers every operation on a fragment but a per-hop block's and looks up no key for it, and the gate passes over every role announced on it and the destination's acceptance. Reassembly restores the payload under the security blocks fragment 0 carries (`Editor::restore_block`, where `Editor::update_block` stripped them), and the whole re-enters the door, where its destination accepts the payload's operations over the whole.

## 4. Reassembly is the remaining full-materialisation island

The streaming pipeline's memory goal (`streaming_pipeline_design.md` §2.5: peak per-bundle resident memory ≈ header size) does not survive fragmented traffic. The current path loads every fragment payload fully, assembles into `vec![0; adu_len]`, editor-rebuilds, `save_data`s the whole ADU, then re-enters ingress with the entire ADU as one `Bytes` stream — where `process_received_bundle` re-parses it and spools the same bytes afresh through `Store::save_stream`, deleting the pre-stored copy afterwards. Peak RAM is roughly twice the ADU, and the payload bytes are written to storage three times across the path (fragments, reassembled save, re-entry spool). The seam is the whole-buffer `process_received_bundle` call in `dispatcher/reassemble.rs` (restart orphan re-admission in `dispatcher/restart.rs` has the same shape).

Until then, an ADU whose total length exceeds `max_bundle_size` is never rebuilt: each fragment is dropped at reassembly as it arrives, Depleted storage, reported, since the whole would be refused by the cap as it re-entered the door, and a fragment or total beyond this host's address space is a failed reassembly rather than a fault.

The streaming-shaped fix: feed the re-entry channel from storage instead of memory — fragment 0's header chain followed by per-fragment payload ranges streamed segment-by-segment — so the gate/parse/spool machinery built for CLA ingress bounds reassembly memory the same way. This is the item most clearly worth deferring until the streaming storage work (sequential spool writes, `Chunk`-shaped storage I/O) has landed, because it wants those primitives.

Note `streaming_pipeline_design.md` §12 lists Reassemble among the blocks that "work on `BundleMetadata`, not raw bytes" — true of the dispatcher block's interface, but not of the path as a whole; this document is the caveat.

## 5. Sibling polling is quadratic

Each delivered fragment triggers `poll_adu_fragments`, a full metadata scan of the sibling set — O(n²) metadata reads across an n-fragment ADU. Harmless at today's fragment counts; worth folding into whatever shape item 4 takes (e.g. an incremental extent tracker keyed by (source, timestamp) rather than a re-scan per arrival, which item 1's insert-time trimming wants anyway).

## 6. Reassembly re-enters the door as a second arrival

The reassembled whole crosses the door as any received bundle does (`dispatcher/reassemble.rs` → `process_received_bundle`, from a fresh `BundleMetadata`): the keyed header pass, the gates, the Ingress chain, the store and dispatch. That is right for the whole's own security operations, which only the whole can carry to its destination's acceptance (item 3), and wrong for everything else reception does: RFC 9171 §5.7 Step 1 has the reassembled bundle proceed from delivery Step 2, so the RFC never receives it again.

The whole is not short of per-hop blocks. Ingress removes an accepted security *operation* from a Previous Node, Bundle Age or Hop Count block, never the block, and discards only a block whose check failed or that it could not accept; the forwarder's writes are egress's (RFC 9171 §5.4), where the per-hop rewrite replaces the blocks by type on whatever block set the bundle carries. So fragment 0 is stored with its per-hop blocks in plaintext, the stitched whole carries them, and the second arrival parses: a clocked bundle needs no per-hop block, and an unclocked one always has its Bundle Age, since a fragment 0 that lost its Bundle Age is deleted rather than stored. What the second arrival gets wrong is reception itself:

- **A second reception report.** The door reports every admitted arrival, so a source that asked for reception reports gets one per fragment, each a bundle (RFC 9171 §5.6), and then one for the reassembled id, which was never on the wire.
- **The gate's expectation on a per-hop block.** A Classifier announcing acceptance on a Previous Node or Bundle Age is met at fragment 0's arrival, where the operation is accepted and removed; at re-entry the same announcement finds the block bare, and the block meets its fate: discarded, or the bundle deleted where the source set "delete bundle if block can't be processed" on it.
- **Fresh metadata.** The whole starts from `BundleMetadata::new`, so the `add_bundle_age` mark set when fragment 0's Bundle Age was discarded is lost, and the whole can leave without an age the source put on.
- **Fragment 0's per-hop values stand for the set.** RFC 9171 §5.9 prescribes it: the whole's Previous Node is fragment 0's sender, its Bundle Age fragment 0's at its forwarding, its Hop Count fragment 0's count, and its siblings' values go with their blocks. The `received_at` anchor above pairs with the first of these.

The shape to take is the completion run's, and the candidates are:

- **A reassembly pass beside `Pass::Receiver` and `Pass::Source`.** The whole crosses the door with no reception report, no per-hop acceptance and no gate expectation on a per-hop block: the destination's acceptance of the operations over the whole, the chain for the decision of record, dedup, store and dispatch, which is RFC 9171 §5.7 from Step 2. The redesign's completion re-run is this pass.
- **Fragment 0's record carried forward.** The whole inherits fragment 0's metadata (the per-hop cache, `add_bundle_age`, the classification) instead of a fresh one. Cheap, but reception is still reported twice and the gate's expectation still fires.
- **A second arrival, documented**, until BIBE supersedes fragmentation.

## Decision

Items 1 and 2 are conformance bugs with a data-loss edge and are fixable now, independent of streaming. Item 3 is resolved (the ingress divert and the stitcher's `Editor::restore_block`). Items 4 and 5 are architecture and should wait for the streaming storage primitives. Item 6 is open: its shape is the completion run's, taken with item 4, and the redesign's completion re-run inherits it. The decision is taken in [`fragment_reassembly_redesign.md`](fragment_reassembly_redesign.md): one fragmentation rework after the streaming bulk, resolving items 1–5 (see its resolution table, where item 6 is listed open).
