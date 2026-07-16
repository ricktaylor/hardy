# Fixing fragmentation

Fragmentation and ADU reassembly work today, but an examination of the reassembly path against the streaming pipeline model (2026-07-16, on the v0.3.0 refactor stack) found conformance gaps, a memory-model hole, and several smaller items. None of them block the streaming work; several of them are best fixed *with* it. This document gathers everything fragmentation-shaped in one place — including items harvested from `TODO.md` — so a coherent decision about fragmentation can be taken once the bulk of the streaming work lands, rather than patching piecemeal.

**Strategic context:** IETF and CCSDS intend to deprecate RFC 9171 ADU fragmentation once BIBE standardises as its replacement (segmented encapsulation — BPSec-clean where fragmentation is not: RFC 9172 §5 forbids adding security blocks to fragments). The machinery below therefore stays correct for RFC 9171 conformance and interop with existing implementations, but the coherent decision this document defers should weigh every item against that horizon: prefer the minimal fix — or an explicit wontfix — for anything BIBE-based segmentation obsoletes.

## What holds today (verified)

The re-entry design is sound and worth keeping: a reassembled ADU re-runs the full ingress pipeline (`dispatcher/reassemble.rs` → `process_received_bundle`) — re-parse, keyed BPSec, expiry/hop gate, dedup, store — so ingress semantics are inherited by construction, and BPSec over the reassembled whole verifies correctly without any special-casing.

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

Hardy's keyed ingress has no fragment exemption: `finalize_with_provider` (`src/bundle/parse.rs`) verifies the deferred block-1 BIB targets against the now-resident payload — for a fragment, that is the partial payload, verification fails, and the fragment is dropped at ingress. Signed-then-fragmented traffic from other implementations (ION fragments readily) therefore never reaches reassembly. This is shared with main — the streaming refactor did not cause it — but the streaming refactor built the natural fix point: the `deferred_bibs` machinery should defer payload-BIB verification *past reassembly* when `primary.flags.is_fragment` is set, since the re-entry pipeline already verifies the reassembled whole correctly. Payload BCBs are unaffected: ingress does not decrypt the payload, so encrypted fragments already pass.

## 4. Reassembly is the remaining full-materialisation island

The streaming pipeline's memory goal (`streaming_pipeline_design.md` §2.5: peak per-bundle resident memory ≈ header size) does not survive fragmented traffic. The current path loads every fragment payload fully, assembles into `vec![0; adu_len]`, editor-rebuilds, `save_data`s the whole ADU, then re-enters ingress as a single `Segment::Final` holding the entire ADU in one `Bytes` — where `process_received_bundle` re-parses it and `replace_data`s the same bytes again. Peak RAM is roughly twice the ADU, and the payload bytes are written to storage three times across the path (fragments, reassembled save, re-entry replace). The two `TODO: Just push the entire bundle into the stream` comments (`dispatcher/reassemble.rs`, `dispatcher/restart.rs`) mark the seam.

The streaming-shaped fix: feed the re-entry channel from storage instead of memory — fragment 0's header chain followed by per-fragment payload ranges streamed segment-by-segment — so the gate/parse/spool machinery built for CLA ingress bounds reassembly memory the same way. This is the item most clearly worth deferring until the streaming storage work (sequential spool writes, `Chunk`-shaped storage I/O) has landed, because it wants those primitives.

Note `streaming_pipeline_design.md` §12 lists Reassemble among the blocks that "work on `BundleMetadata`, not raw bytes" — true of the dispatcher block's interface, but not of the path as a whole; this document is the caveat.

## 5. Sibling polling is quadratic

Each delivered fragment triggers `poll_adu_fragments`, a full metadata scan of the sibling set — O(n²) metadata reads across an n-fragment ADU. Harmless at today's fragment counts; worth folding into whatever shape item 4 takes (e.g. an incremental extent tracker keyed by (source, timestamp) rather than a re-scan per arrival, which item 1's insert-time trimming wants anyway).

## Decision

Items 1 and 2 are conformance bugs with a data-loss edge and are fixable now, independent of streaming. Item 3 is an interop gap whose clean fix (fragment-aware BIB deferral) is small once agreed. Items 4 and 5 are architecture and should wait for the streaming storage primitives. The proposal to decide on: whether to fix 1–3 as a pre-streaming tranche on the v0.3.0 stack, or batch everything into one fragmentation rework after the streaming bulk lands.
