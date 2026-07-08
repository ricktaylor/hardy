# bpa TODO

## RFC 9171 §5.9 material-extents reassembly (overlapping fragments)

### Background

`Store::reassemble()` (`src/storage/adu_reassembly.rs`) requires the received fragments to tile `[0, total_adu_length)` exactly: contiguous, non-overlapping and complete. Overlapping fragment sets are rejected and the fragments dropped as `ReassemblyResult::Failed`.

RFC 9171 §5.9 is more permissive: overlapping fragments are legal on the wire (e.g. the same bundle refragmented differently on divergent paths), and a conformant reassembler computes each arriving fragment's "material extents" — the byte ranges not already covered by previously received fragments — completing when the material extents concatenate to the full ADU. First-received bytes win; overlap is trimmed, not rejected.

Hardy has never accepted overlap: the pre-tiling-check code also failed overlapping sets (payload-length sum ≠ total), except for the length-sum coincidence that silently delivered a corrupt ADU (2026-07-08 review findings #1/#4). The tiling check makes rejection deterministic and safe, but Hardy remains non-conformant for legitimately overlapping fragment sets.

### What full §5.9 support needs

- `FragmentSet` must hold *trimmed* ranges decided at insert time by arrival order: on insert, clip the new fragment's payload range against the extents already covered (possibly splitting it), rather than keying whole fragments by raw offset.
- The completeness gate in `poll_fragments()` (`adu_totals >= total_adu_len`) must sum material extents, not raw payload lengths, or completion fires early on overlapping sets.
- The copy loop in `reassemble()` then slices each stored payload sub-range; the tiling invariant holds by construction.
- §5.9 requires the reassembled ADU to replace the payload of the fragment whose material extents include offset zero — the current "fragment 0" special-casing needs re-deriving from material extents, not from a raw offset-0 key.

## Deletion status reports on reassembly failure

When reassembly fails (`ReassemblyResult::Failed`), `Store::adu_reassemble` deletes the held fragments directly against storage (`delete_data` + `tombstone_metadata`) and the dispatcher's `Failed` arm returns without action — no deletion status reports are generated. RFC 9171 §5.10 says a deletion status report SHOULD be generated per deleted bundle (each fragment is its own bundle, reported to its own report-to EID with its fragment offset/length) when the report flag is set and reporting is enabled.

The fix is plumbing, not policy: `adu_reassemble` should hand the fragment `Bundle`s back on failure instead of consuming them, so `Dispatcher::reassemble` can route each through `drop_bundle(bundle, reason)` (which already does the flag-gated `report_bundle_deletion` + delete). Reason-code selection per failure mode needs deciding: `DepletedStorage` fits the length-not-addressable case; coverage gaps/overlaps have no exact RFC 9171 reason code (`NoAdditionalInformation` or `BlockUnintelligible` are the candidates).

