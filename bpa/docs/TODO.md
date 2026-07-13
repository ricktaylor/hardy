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

## Ingress status-report conformance (invalid bundles, duplicates, expired arrivals)

RFC 9171 pins the reception report's reason code: §5.6 Step 2 prescribes "No additional information" (Step 4's "Block unsupported" is the only other reception-report reason), and a parse/CRC failure routes through the §5.10 Bundle Deletion procedure, whose deletion report is what cites "Block unintelligible". The ingress invalid-bundle path instead sends a single reception report carrying the failure reason and no deletion report. This shape is shared by main and refactor/cla-streaming, and survives on refactor/parse and refactor/metadata in both invalid paths there (`parse_headers` failure and post-drain `finalize` failure) — only the lifetime/hop early gate on those branches emits the conforming reception-then-deletion pair. Fix when the parse refactoring is picked up post-v0.2.0: reception report with "No additional information", deletion report citing the failure reason.

Duplicates: RFC 9171 specifies no duplicate-bundle processing at all (the word does not appear), so §5.6 read literally reports reception on every arrival, including replays. Decision: duplicates SHOULD get a reception report — a sender may be intentionally repeating a bundle probing for status-report ACKs. refactor/parse already reports reception before the duplicate check; main reports after, so a refused duplicate is silent. Adopt the report-before-dedup ordering with the refactor.

Expired arrivals are the deliberate exception: a bundle that arrives already expired is treated as if it never arrived — no reception report, no deletion report, no metadata entry — rather than amplifying already-dead traffic into report bundles (`fix/mem-storage-watermark` implements this in the ingress expiry gate; the refactor/parse early gate currently reports and should adopt the same silence for the lifetime case when picked up). Bundles that expire *in custody* are unaffected: the validity filter, reaper, and `drop_bundle` paths still generate §5.10 deletion reports citing "Lifetime expired".

## Storage backend metrics (common + bespoke)

An audit (2026-07-10, while defaulting bpa-server to sqlite/localdisk) found the persistent storage backends emit no metrics at all — none of sqlite-storage, localdisk-storage, postgres-storage, or s3-storage even depends on the `metrics` crate. Their only observability is the `instrument`-feature tracing spans. Production nodes on the new persistent defaults therefore have no storage operation counts, error counters, or latencies; only the in-memory backends carry store-level metrics today (`bpa.mem_store.*`, `bpa.mem_metadata.*`, documented in `docs/user-docs/operations/observability.md`).

Two layers of work:

**Common metrics in the `Store` wrapper** (`src/storage/store.rs`) — every operation flows through it, so instrumenting there covers all backends uniformly and keeps the backend crates metrics-free for the common cases. Proposed: `bpa.storage.ops` (counter), `bpa.storage.errors` (counter), and `bpa.storage.op.duration` (histogram), labelled `store = metadata|bundle` and `op = load|save|replace|delete|insert|tombstone|get|poll` (the `poll_*` variants folded into one label value to bound cardinality).

**Bespoke per-backend metrics** where only the backend knows the truth: sqlite database/WAL file size, localdisk store-directory bytes and file count, postgres connection-pool stats, s3 request retries. These need either per-crate `metrics` emission or a `stats()`-style trait extension sampled periodically — a trait-surface decision to make when the work is picked up.

Documentation: a new backend-agnostic "Storage Operations" table in `docs/user-docs/operations/observability.md`, plus per-backend tables as bespoke metrics land.
