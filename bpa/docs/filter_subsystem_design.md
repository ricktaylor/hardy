# BPA Filter Subsystem Design

The embedder's extension seam on the bundle pipeline: three filter kinds, registered in construction-frozen packs, run inline at four hook points. A registered filter is trusted code with plaintext read access — block bodies BCB-decrypted on request, the payload's included whenever it is resident; only a Rewriter's *edits* are scoped, to extension blocks.

> **Status.** This document describes the implemented registration surface, filter kinds, and engine (the filter redesign's Phase 2, per [`refactor_plan.md`](refactor_plan.md)). Two settled parts of the design are not yet implemented and are flagged where they appear: the repositioning of the Ingress chain onto the pre-drain gate together with [restart re-admission](#restart-re-admission) (Phase 3), and the [scanner component](#payload-inspection-is-a-component-not-a-filter-kind) (Phase 4). The `class`/`route_key` classification fields arrive with the policy and routing tranches ([`policy_subsystem_redesign.md`](policy_subsystem_redesign.md), [`routing_table_redesign.md`](routing_table_redesign.md)).

## The governing constraint: a closed-source server on an unmodified open `bpa`

**In short: the pluggable filter system exists so a custom `bpa-server` can update the BPA's processing *rules* in a licence-clean manner** — against an unmodified, Apache-2.0 `bpa` consumed as an ordinary Cargo dependency, never a fork. A fork is ruled out not by the licence (Apache-2.0 would permit one) but by what it commits to: re-merging every upstream fix and release, forever. The registration seam is what makes "unmodified" sustainable, and the rules/processing split is exact: filters are the rules an embedder supplies; the pipeline is the processing the BPA owns.

It must be possible to build an alternate `bpa-server` binary in a closed-source repository that depends on the open-source `bpa` crate **completely unmodified** and have that closed binary inject its own proprietary filters. A `#[cfg(feature)]`-gated filter cannot serve a closed extension (adding the cfg call-sites means editing `bpa`, i.e. forking it), so anything proprietary reaches an unmodified `bpa` through exactly one door: a **public registration API** — `bpa` exposes the filter traits, the closed `bpa-server` implements them and registers its filters at construction. The extension is **link-time, not run-time**: the Trait + register pattern Hardy already uses for CLAs, Services, and Routing Agents.

## Pipeline, not filters: the two-tier split

The word "filter" invites conflating two different things (netfilter-styled designs, with registered read/write tables at every hook, do exactly this):

1. **The BPA's own processing** — spec-mandated and core checks. That some of these can run in parallel is an *execution detail*, not an extensibility feature.
2. **Extension points** — places where the embedding application hangs policy the open crate cannot know about. Only this needs the trait + registration surface.

The built-in checks are therefore **not registered filters — they are the pipeline**. The registration API exists for exactly one reason (closed-source embedding) and nothing internal passes through it. This is the correct reading of the netfilter analogy: conntrack and defrag hook the very same kernel points as iptables rules, but they are not rules — they are kernel processing at fixed priorities. The tables and chains are purely the *user's* surface, empty by default.

The per-layer configurability answer falls out:

- **Built-ins**: runtime *configuration* (on/off toggles, parameters on the `BpaBuilder`). Never registration, never reorderable — spec ordering is unviolable through the public API by construction. The RFC 9171 validity checks (`primary_block_integrity`, `bundle_age_required`, strict defaults) and the IPN legacy re-encode (`ipn_legacy_peers`) are this tier.
- **Embedder chains**: construction-time registration, then **frozen**. Link-time embedding needs nothing more, and the frozen chain keeps the hot path lock-free.
- **Remote or dynamic registration: excluded by design, permanently.** Filters are in-process, link-time extensions only — a gRPC filter puts an RPC round-trip on the per-bundle hot path, and dynamic (un)registration would put a runtime-mutable registry on it. The remotable, runtime-registrable tier is the *component* tier; anything that needs those properties is a component, not a filter. Consequence: **the filter chain is locked between restarts**, making a restart the policy application boundary — see [Restart re-admission](#restart-re-admission).

Litmus test for anything ambiguous: *could an embedder legitimately want this absent or reordered?* No → pipeline code. Yes → filter.

This sits inside a wider two-tier extension model:

- **Runtime registry + sink** — for *components*: CLA, Service, RoutingAgent, AdminRecord (see `docs/acme_design.md`), keys. Register/unregister while running, gRPC-remotable, authorisation at the gRPC layer.
- **Construction-frozen registration** — for *policy supplied as code*: the filter chains, and the FlowController factories (the policy/queue tranches' seam). In-process, link-time, never remotable: policy code sits on the per-bundle hot path.

Anything that appears to want runtime registration is evidence it is a component wearing a filter costume.

## Hook points: the pipeline enumerated

The evidence base for the taxonomy. Every processing point in the in→out pipeline, classified by (a) read or write, (b) externally pluggable. Stage names follow the processing blocks of [`queue_architecture.md`](queue_architecture.md); function references are current `dispatcher/` code. The ★ hook rows show the *designed* positions; today the registered Ingress chain runs post-store in `ingress_bundle` (before the `Dispatching` checkpoint), and Phase 3 moves it onto the pre-drain gate where the config-gated built-ins already sit. The other three hooks are at their designed positions.

**In from a peer** (Ingest block — `receive_bundle` → `process_received_bundle` → `ingress_bundle`):

| Processing point | R/W | Pluggable? |
|---|---|---|
| CLA deframing → segment stream (`stream::Segment` via `Sink::dispatch`) | write (assemble) | already — CLA trait |
| Structural header parse, canonical enforcement (`parse_headers`) | read (reject) | no — spec |
| Keyed header verify: BIB verify, BCB decrypt for extraction, NoKey liveness | read (reject) | keys already via KeyProvider/KeySource |
| Extension fields → metadata wire cache | write (meta) | no |
| Pre-drain gate: lifetime / hop exhaustion (`gate_reason`) + the config-gated RFC 9171 checks (`rfc9171_gate_reason`) | read (reject) | no — spec/config |
| **★ Ingress hook** — registered Verifiers ∥, then Classifiers | read + annotate (delta) | **yes — the hook** (headers + metadata, no payload) |
| Payload drain/spool | write (accumulate) | no |
| Finalize: deferred block-1 BIB, §5.1.1 failure-drop, removal rewrites | **write (bytes)** | no — parser/BPSec owns |
| Persist; reception report (§5.6, before dedup); dedup | write | storage trait; reports fixed |
| Enqueue to Dispatch | queue op | no |

**In from a local application** (Originate block — `local_dispatch`/`local_dispatch_raw`):

| Processing point | R/W | Pluggable? |
|---|---|---|
| Build via `Builder`, or parse + validate raw bytes (same parser — security boundary), inline lifetime/hop admission check on the raw path | write (create) / read (reject) | already — Service trait |
| **★ Originate hook** — registered Verifiers ∥, then Classifiers | read + annotate (delta) | **yes — the hook** (pre-store, in-memory; a Drop returns its reason to the originating service) |
| Store + dedup | write | no |
| Enqueue to Dispatch | queue op | no |

**The middle** (Dispatch block):

| Processing point | R/W | Pluggable? |
|---|---|---|
| RIB lookup → Drop / AdminEndpoint / Deliver / Forward(peer, next hop) / Wait | write (queue assignment) | already — RoutingAgent |
| Admin records → Admin block (no Deliver hook — see below) | — | already — AdminRecord registry |
| Fragments → Reassemble block → re-enter Ingest processing | write | no |
| Peer-seat FlowController (egress scheduling) | read (schedule) | no — a fixed point (tc/qdisc in the netfilter analogy, not iptables) |
| Waiting / WaitingForService parking (gated queues) | queue op | no |

**Out to a peer** (ClaSend block — `forward_bundle`):

| Processing point | R/W | Pluggable? |
|---|---|---|
| Load from store | — | no |
| **★ Egress hook** — registered Rewriters, seq | **write (extension blocks)** | **yes — the hook** (in-memory, per transmission attempt, re-run with fresh context on re-route; payload/primary/BIB/BCB excluded by the editor handle) |
| Per-hop writes: PreviousNode insert, HopCount increment, BundleAge update, and the config-driven IPN legacy re-encode (`update_extension_blocks`) | **write (extension blocks, primary for the re-encode)** | no — spec §4.4.x + config; the fixed tail of the rewrite stage, superseding Rewriter edits to the blocks it writes |
| BPSec-egress seam | **write (bytes)** | no — fixed, security-policy/KeyProvider-driven (designed; not yet built) |
| CLA: fragmentation + framing | write | already — CLA trait |

**Out to a local application** (Deliver block):

| Processing point | R/W | Pluggable? |
|---|---|---|
| Load from store | — | no |
| **★ Deliver hook** — registered Rewriters (seq, transport-block strip), then Verifiers ∥ | read (reject) + **write (extension blocks)** | **yes — the hook** (runs for every local delivery, before payload decrypt; edits are observable on the raw-`Service` path only) |
| BCB payload decrypt — produces the ADU | write (plaintext) | no — normal BPA functionality |
| `service.on_deliver` | — | already — Service trait |
| Delivery report; delete | write | no — spec |

Two findings carry the whole design:

- **Payload and primary-block writes are exclusively fixed, spec-owned machinery** — the parser at ingress, BPSec at egress, decrypt at deliver, framing in the CLA; their ordering is RFC-mandated and none is a plausible external plug point. The one pluggable mutation point is the extension-block rewrite at ClaSend — where the pipeline already edits blocks per hop — and it is pluggable *by scope* (extension blocks only, via the Rewriter's editor handle), not by exception.
- **Every input-side hook is read + annotate.** Inputs never mutate wire bytes; the mutating hook capability exists only on the output side, where each hop's wire form is derived.

## The filter kinds

Three kinds, defined in `bpa/src/filter/mod.rs` (`Verifier`, `Classifier`, `Rewriter`; rustdoc owns the signatures). Every invocation receives its kind's context — `VerifyContext`, `ClassifyContext` or `RewriteContext` — which lends the *wire* bundle through bpv7's own types — the `hardy_bpv7::Bundle` (primary block and structural block index) and a `hardy_bpv7::reader::Reader` over the block bodies — and the BPA-local record state (provenance, wire cache, classification, annotation slots) through a **separate getter** (ruling 2026-09-09): the immutable wire form and the record's mutable annotations are deliberately never one object, so the two cannot be confused. A `RewriteContext` also carries the Rewriter's `Boundary` and owns the `ExtensionEditor` its edits go through. The contexts are the extension point for filter inputs: a new input is a new getter, not a breaking change to every implementor, and the getters keep the reader's lifetime out of implementors' signatures. The reader is a `bpsec::DecryptingReader` over the resident source bytes, the decoded BCB OperationSets, and a `KeySource` resolved through the existing KeyProvider seam. A block body reads as an `Availability`: the plaintext or decrypted bytes when available, otherwise the state that says why not; a BCB-covered block the node holds no key for (`NoKey`) is the filter's no-match path, never an error it must handle, and `ReaderExt::extract` folds every unavailable state to `Ok(None)`. The reader memoises: one serves every filter that sees the same bytes (a whole input pass, the Verifiers closing the Deliver chain) and each Rewriter gets its own, so a covered block is decrypted at most once for the filters sharing it. Verifiers and Classifiers return the shared `Verdict` enum, so the drop path — and its status-report reason — is identical everywhere; a Rewriter returns nothing.

| Kind | May do | Mutates | Execution | Hooks |
|---|---|---|---|---|
| **Verifier** | `Continue` / `Drop(reason)` | nothing | parallel (independence contract) | Ingress, Originate, Deliver |
| **Classifier** | `Continue(MetadataDelta)` / `Drop(reason)` | metadata, via delta | sequential | inputs only |
| **Rewriter** | edit extension blocks (no verdict) | extension blocks, via bpv7's `ExtensionEditor` | sequential | **Egress + Deliver** |

**A Rewriter has no verdict (ruling, 2026-10-02).** It edits the bundle or leaves it as it is, and never drops it. At Egress a drop deletes the bundle when the honest meaning is "this hop can't take it in this form"; at Deliver, refusal is the job of the Deliver Verifiers, which gate what the Rewriters leave.

**The mutation boundary is the block, not "bytes".** Extension blocks are BPv7's own extensibility surface — where the protocol itself says new wire-visible behaviour lives — so a filter system whose purpose is licence-clean custom processing rules must be able to add, modify, and remove them. What filters can never touch: the **payload block** (the application's data, owned end-to-end by the application, BPSec, and the CLA), the **primary block** (immutable per RFC 9171), and **security blocks** (BIB/BCB — the BPSec seams' monopoly). The Rewriter edits through bpv7's `ExtensionEditor`, which exposes exactly the permitted operations — `insert`/`replace`/`remove` of extension blocks — and refuses out-of-scope targets with a typed error at call time, so payload-purity is enforced by the type rather than by code review. It refuses edits to blocks under existing BPSec coverage (this node is not their security source), conservatively including blocks whose coverage is unprovable because a BIB is undecryptable; and it refuses a target block number the bundle does not have, or one an earlier operation removed, where the owner `Editor`'s `remove_block` would silently succeed — a scoped-policy layer fails loudly. A block the Rewriter inserted is a valid target for its own later `replace` or `remove` in the same invocation. Operations are validated as they are called, so a refusal is the Rewriter's own no-match path; the engine materialises the accumulated edits when the invocation returns.

The two mutating kinds are duals by scope: the **Classifier** (inputs) writes *node-scoped* annotations — metadata this node's own downstream consumes; the **Rewriter** (egress) writes *network-scoped* annotations — extension blocks the next hops consume. A rule whose effect is local needs a delta; a rule whose effect must travel needs a block.

All three kinds are payload-free and therefore **streaming-immune**: at ingress, Verifiers and Classifiers need only the header blocks (plus any declared payload peek), and when the full streaming gate lands (`streaming_pipeline_design.md` §5.4/§5.7) they ride it unchanged; the Rewriter edits header blocks, which are resident at ClaSend where the per-hop rewrite already operates, while the payload streams past untouched. There is no late ingress pass — an early/late Ingress split would exist only to serve in-pipeline payload inspection, and with none there is nothing a late pass could see that the pre-drain pass cannot.

The Classifier returns a *delta* rather than taking `&mut metadata` deliberately: the engine applies it, filters never touch `bundle.metadata` directly, the boundary stays clean for closed-source implementors, and delta application is idempotent — which the queue architecture's at-least-once semantics require of every processing block. A Classifier **sees the deltas applied by preceding links** of the same pass: the engine applies each delta before the next invocation. The reader/metadata split above is what makes this cheap and unambiguous — the reader lends only the wire form (invariant for the whole pass, one construction), while the `metadata` argument is re-borrowed per link over the advancing record state; a Classifier alters metadata through its delta and never the bundle itself. (An accumulate-then-apply variant — every link observing gate-time state, deltas merged after the pass — was considered and rejected 2026-09-09: per-link visibility is the designed behaviour.)

The Rewriter's execution model is **in-memory, per transmission attempt** — exactly the semantics of the built-in per-hop writes that follow it. The stored bundle remains as received (post-parser canonicalisation); each hop's wire form is derived fresh at ClaSend and never written back. That single choice is what keeps the rest of the design intact: no persisted filter mutations means no crash window between rewrite and enqueue, restart re-admission stays metadata-only, and a re-routed bundle is re-prepared for its new peer with fresh context.

**Filters are trusted code.** No view type mediates byte access, because filters are in-process, link-time, supplied by the embedder — the registration seam is a *licence* boundary, not a *privilege* boundary, and there is no security boundary inside a process for a wrapper type to enforce. What is real is the **residence contract**: every invocation sees the header blocks plus the first P declared peek bytes, while payload residence beyond P is never guaranteed (hook position, streaming state). The reader's optional returns express the variability uniformly. A filter that *depends* on payload bytes has written itself out of streaming-immunity, and the design's answer to payload-hungry processing remains the component tier ([below](#payload-inspection-is-a-component-not-a-filter-kind)).

## Hooks and the matrix

| Hook | Processing block | Position | Verifier | Classifier | Rewriter |
|---|---|---|:--:|:--:|:--:|
| **Ingress** | Ingest | designed: pre-drain, pre-store (at the gate); today: post-store, pre-dispatch | ✓ | ✓ | — |
| **Originate** | Originate | pre-store, in-memory | ✓ | ✓ | — |
| **Egress** | ClaSend | after load, before the per-hop writes and BPSec | — | — | ✓ |
| **Deliver** | Deliver | before payload decrypt | ✓ | — | ✓ (transport-block strip) |

**Classifier is inputs-only.** Classification annotates metadata that the node's *own downstream* consumes — the traffic class whose properties drive dispatch weighting, egress contracts, eviction, and table selection, plus the routing key and any annotation slots. There is no downstream inside the BPA past an output boundary, so an Egress or Deliver Classifier has nothing to feed.

**There is no Egress Verifier (ruling, 2026-10-02).** Everything a filter can read about the bundle — content, provenance, classification — is already available at Ingress, where a Verifier runs once, before the bundle is stored. The one input unique to Egress is per-attempt: the next hop routing chose. And a Drop is the wrong answer to "not via this hop": it deletes the bundle where the right response is another hop, or waiting. Each plausible use has a better home. Per-adjacency policy ("class C must not go to peer P") is routing's, through per-class route tables and an input Classifier's `route_key`. Content policy (DLP, export control) is an Ingress Verifier's, since content does not change between attempts. "Must be protected on this link" is the egress BPSec seam's policy, which a pre-BPSec gate could not see anyway. An invariant on the Rewriters' output belongs to the trusted Rewriter that writes it, and an audit of what leaves is tracing's. Deliver keeps its Verifiers: it is where this node accepts the bundle's security operations as their destination (e.g. "service S accepts only integrity-protected bundles"), and a local destination has no alternative route, so a Drop is the honest refusal. With no Verifier and no Rewriter verdict, nothing at Egress drops a bundle.

**Rewriter is egress + deliver — the two output boundaries.** Inputs never rewrite — incoming wire bytes are canonical truth, rejected at parse, never rewritten (`streaming_pipeline_design.md` §5.2.2) — and a locally-originated bundle's blocks are the Builder/service tier's to assemble. Wire preparation for the next hop belongs at the one point with next-hop context, per transmission attempt; that also covers locally-originated traffic, since Originate→Egress passes through ClaSend like everything else. Deliver is the *dual*: it strips **transport-scoped** extension blocks — network QoS, custody, the per-hop plumbing ("transport headers", to borrow HTTP's hop-by-hop/end-to-end split) — so a terminating bundle hands *content* to the application, not network bookkeeping (a security property too: Previous Node and internal QoS policy do not leak to an untrusted app). The chain runs for every local delivery — a Verifier's Drop verdict applies to `Service` and `Application` deliveries alike — but only the raw-bundle `Service` path observes the rewritten blocks: the payload-only `Application` path receives the decrypted payload alone, so a Rewriter's edits are invisible there. Because the Rewriter edits extension blocks and never the payload, the Deliver invocation sits *before* the payload's BPSec decrypt and uses its `KeySource` to decrypt any extension block it must inspect. Next-hop context is Egress-only (a delivering bundle has no next hop), so it rides the `Boundary::Egress` variant of the Rewriter's context.

**There is no FORWARD hook.** The pipeline has exactly one out-to-peer processing point — locally-originated and transit traffic converge before ClaSend — so the output hook is netfilter's POSTROUTING, not its FORWARD. Netfilter needs a separate FORWARD chain because flat rule tables cannot ask "is this transit?"; our filters are code with metadata access, and the persisted provenance (`origin` is `Ingress`) at Egress *is* the transit predicate. The two things a FORWARD hook would add beyond that predicate have better homes: acting once per routing decision is an Ingress matter (provenance never changes after ingest), and changing where a bundle goes is the RoutingAgent's job — a filter that redirects is a routing agent in a costume. The hook keeps the name **Egress** deliberately: it does *not* have netfilter-FORWARD semantics (it sees originated traffic too), and naming it Forward would import the transit-only intuition.

**The admin path is hook-free by design.** Admin records addressed to this node terminate in the Admin block, whose extension surface is the **AdminRecord registry** (registry + sink keyed by record type, with record-type-claim authorisation at the gRPC layer — `docs/acme_design.md` §4). Filters never intercept the control plane's terminal processing; admin bundles still cross Ingress like any other bundle, so boundary policing applies.

## Execution: the inline engine

The frozen chains run **synchronously, inline on the calling task** (`filter/engine.rs`). Two facts force this and one payoff justifies it:

- Filter invocations are synchronous by trait signature — async methods would exist only to serve callout-style filters (gRPC policy engines, database lookups), which the two-tier split places in the component tier; policy-as-code on the hot path has no await points.
- The filters' `DecryptingReader` borrows the caller's decoded block map, buffer, BCB OperationSets, and key source — borrows that cannot cross a spawn boundary — so an invocation cannot migrate across tasks anyway.

"Parallel" Verifiers is therefore an **independence contract** — no ordering, no cross-talk between the Verifiers of one hook — not a spawning strategy, so the engine needs no task pool and no pool-deadlock reasoning. An empty chain costs one branch: nothing is parsed, nothing is allocated, so hooks nobody extends are near-free. Each Rewriter that edits costs a full copy of the wire form (its edits are materialised before the next link reads them), so M editing Rewriters hold up to M bundle copies at peak, per attempt — the whole-buffer interim that the streamed egress Transformer (`streaming_pipeline_design.md` §6.1, Phase B) replaces.

Two engine behaviours are load-bearing for the dispatcher's claim discipline:

- **Every runner returns the bundle to the caller** — with every verdict (`ChainOutcome`; the Egress runner, which has none, returns the rewritten pair), and on an input chain's error path (`(Bundle, error)`). The output hooks run inside a claimed window (`ForwardAckPending`/`DeliveryAckPending`), and the site that claimed the bundle resolves the claim with the same record on every exit — no re-fetch, no restore path, conditional swaps stay clean.
- **Each Rewriter sees its predecessors' edits**: the engine materialises every invocation's accepted edits into the wire form (and re-indexes the block map) before the next invocation reads it — the mutation-side mirror of the Classifier delta rule.

**A Rewriter execution failure is a fail-stop, not an error (ruling, 2026-09-09).** The engine's recoverable error path exists for exactly one event: an input chain's bytes failing the engine's own decode pass. A *Rewriter* failure — an edit the editor accepted but could not materialise, or an edit whose materialised bytes do not re-parse — is treated like a storage fault: the rewrite was meant to work and has not, so the correctness of every subsequent processing step is undefined, and there is no error a caller could react to appropriately (parking and retrying re-runs the same broken Rewriter forever; skipping it transmits a bundle the operator's own policy code failed to produce). The engine panics naming the failing link's pack-prefixed label, and the panic aborts the process (every pipeline task runs under the `TaskPool` watchdog); the message is formatted only on that path. Rejected alternative, so it is not relitigated: a labelled error type (`RewriteError { label, source }`, the 2026-08-28 patch's `Error::Rewrite` shape) surfacing through `ChainOutcome`'s error path — it turns an undefined-state condition into a parkable status transition the dispatcher then has to have an answer for. Note the boundary: a Rewriter *refusal* (an edit rejected by the `ExtensionEditor` at call time) is the filter's own no-match path and entirely healthy; only `editor.finish()` failing or the materialised bytes failing to parse is the fault. The editor keeps that boundary where the bundle is attacker-chosen: whatever a receiver would reject — a `report_on_failure` flag on an administrative-record or null-source bundle and an unrecognised CRC type, which the parser rejects, and Previous Node, Bundle Age, or Hop Count data that does not decode, which BPA ingress rejects — is refused at call time with the rejecting check's own error, so a correct Rewriter meets a refusal, never the fail-stop.

**An output chain's decode failure is fatal too (ruling, 2026-10-02).** Anything that gets into the system is validated at the door, so a stored bundle the BPA later fails to process has met a BPA bug or storage corruption, and both are fatal, like a storage fault. The Egress and Deliver chains decode bytes that were validated at ingress and read back from storage, so a decode failure there panics, as a Rewriter failure does. Parking the bundle instead would only re-run the same failure at every routing event or registration until the bundle expired.

## Where bytes change

**Payload and primary-block mutation is exclusively fixed machinery**; extension-block mutation has exactly one pluggable point, inside the egress sequence:

```
load from store                          (stored bundle = as received; source of truth)
  └▶ registered Rewriters (seq)          extension-block add / modify / remove — THE pluggable mutation point
  └▶ built-in per-hop writes             PreviousNode / HopCount / BundleAge — fixed, spec §4.4.x
                                         + the config-driven IPN legacy re-encode (ipn_legacy_peers)
  └▶ BPSec-egress seam                   add BIB/BCB per security policy — fixed, KeyProvider-driven (designed; not yet built)
  └▶ CLA                                 fragmentation + framing — fixed link adaptation
```

**The per-hop writes follow the Rewriters (ruling, 2026-10-02).** The per-hop writes are the BPA's RFC 9171 §5.4 duties for this hop, and Bundle Age is to be increased "at the last possible moment before the CLA initiates conveyance", so they run last, after every Rewriter and before the BPSec seam, which then protects their output as well as the Rewriters'. They therefore supersede a Rewriter's edits to the blocks they write: the Previous Node always; the Hop Count when the bundle arrived with one this node could read and can update; the Bundle Age when the bundle arrived with one or has no creation clock. A Rewriter can no longer strip a clockless bundle's Bundle Age, and the Rewriters read the stored per-hop blocks, matching the record's extension-field cache. A per-hop block a Rewriter adds where the BPA writes none travels as the Rewriter wrote it. The Rewriters still sit before the BPSec seam, so the blocks they add or modify can be signed per security policy.

**The Hop Count increment yields to BPSec (ruling, 2026-10-05).** RFC 9171 §4.4.3 makes the increment a SHOULD, so where the per-hop write cannot safely update the Hop Count block — BCB-covered, beside an encrypted BIB that may cover it — the block travels unchanged with its security operations rather than holding the bundle back. The Previous Node and Bundle Age writes are MUSTs: a block of either kind under such coverage parks the bundle until the per-hop BPSec handling replaces the park.

The IPN legacy re-encode (3-element `ipn` EIDs re-encoded 2-element for peers that require the older form) is a built-in of this stage rather than a registered filter, by this design's own taxonomy: it rewrites the *primary block*, which is out of every Rewriter's scope by construction, and it is per-hop wire adaptation, not embedder policy. It is driven by `BpaBuilder::ipn_legacy_peers` (EID patterns matched against the resolved next hop) and — like every rewrite in this stage — applies only to the transmitted wire form: the record's own primary is never replaced, so id-keyed resolution after transmission (tombstones, dedup) always compares like with like. A BPSec operation that covers the primary block, as its target or through its scope (RFC 9173's default scope includes the primary in every operation's input), cannot survive the re-encode, so such a bundle is dropped at a legacy next hop with `UnexpectedSecurityOperation`, which RFC 9172 §7.1 notes may identify a security-policy misconfiguration. bpv7's editor decides it: it refuses the primary edit while a BIB targets the primary or any remaining operation's scope includes it, an unreadable encrypted BIB counting as one, and the stage maps that refusal to the drop, so the BPA reads no security context's parameters. The refusal is judged after the per-hop writes, so a BIB that covered only the blocks those writes replace, stripped and emptied by them, no longer stands in the way.

The fixed byte-owners elsewhere are unchanged:

- **Ingress**: the parser — canonical rejection (never rewrite incoming wire bytes), BPSec verify/decrypt, the RFC 9172 §5.1.1 removal cascade.
- **Deliver**: BCB payload decrypt — normal BPA functionality.
- **Re-targeting** (BIBE, tunnelling, overlays): RIB-selected **virtual CLAs**. The carrier gets a new destination, so the next lookup makes forward progress; chaining is the sequence of carrier destinations; loop protection is ordinary hop-count/age.

There is no payload-rewriting egress Transform. The slot such a filter would reserve belongs to BPSec (a fixed seam), and its other tenants are payload operations with better homes: compression and framing are the CLA's, transcoding/redaction are rewriting gateways (components), aggregation is application-layer or BIBE-shaped encapsulation. The Rewriter is not a Transform — it is scoped to extension blocks, extends a stage that already mutates them, and its output remains subject to the fixed BPSec seam behind it.

## BPSec roles at the boundaries

The fixed BPSec machinery named above follows the RFC 9172 §3.2 role model, mapped onto the same boundaries the hooks sit at. bpv7 is policy-free mechanism (*check* functions decrypt/verify and report; *edit* functions remove/replace blocks); bpa owns policy and call sites. The egress half of that machinery, adding BIBs and BCBs by policy at Originate and Forward, is designed but not yet built: today the BPA verifies and decrypts, and signs or encrypts nothing on the way out. A node holding a key is **at least a Verifier**: it validates, keeps valid blocks encrypted, and applies the §5.1.1 failure-drop (removing corrupt ciphertext only, exposing no plaintext). An **Acceptor** additionally *consumes* a valid operation (decrypt + strip), deferred to Deliver/Egress so storage stays encrypted; the waypoint-acceptor call site is the fixed BPSec-egress seam in ClaSend — built-in pipeline, never a registered filter.

| Boundary | BPSec role | Checks | Edits | On failure |
|---|---|---|---|---|
| **Originate · Service trait** | caller = Source for payload; BPA = Source for extension blocks | structurally validate; verify caller's security | add BPA extension blocks and secure them | reject to caller |
| **Originate · Application trait** | BPA = Source | validate built bundle | build; add BIB/BCB per policy | reject to app |
| **Receive** (CLA → BPA) | BPA = **Verifier** | parse; verify BIBs pre-drain; decrypt extension blocks to read; classify unsupported; payload BIBs post-drain via `deferred_bibs` | failure-drop corrupt non-payload block + security blocks; drop `delete_block_on_failure` unknowns; keep valid encrypted | block → drop block; bundle-level → drop bundle; NoKey → keep |
| **Deliver · Application trait** | BPA = **Acceptor** | decrypt payload (`block_data`); optionally verify payload BIB | consume → transient plaintext to app; bundle deleted after | payload fail → drop bundle; NoKey → watch |
| **Deliver · Service trait** | service is the Acceptor; BPA passes through | none | none — hand raw encrypted bytes to service | service's responsibility |
| **Forward** (BPA → CLA) | Forwarder (default); waypoint Acceptor/Source by policy | none by default; waypoint acceptor decrypts/verifies its op | per-hop blocks; optionally waypoint-accept at the BPSec seam → decrypt + strip + §5.1.1 | waypoint fail → §5.1.1 (drop block; payload → drop bundle) |

## Payload inspection is a component, not a filter kind

There is no Inspector kind (DPI: read the payload to drop or annotate). The honest use cases — AV scanning at a domain gateway, DLP at egress, content-based classification, audit capture, ADU protocol validation — all fail the in-pipeline test, for three structural reasons:

- **Encryption**: in a BPSec deployment, transit payloads worth scanning are BCB ciphertext. Payload inspection is only meaningful where plaintext exists (a security-acceptor gateway, or delivery), so it is a deployment feature, not a pipeline feature.
- **Latency**: real scanning is slow (signature updates, sandboxing). An inline hook blocks a processing block; store-and-forward *already* parks bundles as normal operation — "hold until an external process renders a verdict" is native DTN behaviour.
- **Isolation**: AV/DLP engines are large hostile-input parsers. They belong out-of-process, in the component tier (remotable registry + sink), not linked into the BPA's address space via a trait.

Precedent: netfilter itself refused in-chain DPI — Linux queues packets to userspace via NFQUEUE and gets a verdict (+mark) back; proxies hand payloads to external scanners over ICAP. Nobody embeds the engine.

Two component shapes in Hardy terms (**Phase 4 — neither is built yet**):

1. **Queue/verdict (the NFQUEUE analogue — preferred).** Under the queue architecture, forward-to-scanner is an *enqueue to a scanner-owned queue*; the scanner is a registry+sink component that consumes bundle bytes and returns a verdict — release with an optional classification delta, or drop with a reason — which re-enqueues to Dispatch. No re-ingress, no dedup collision, parking is a queue doing what queues do, and the component is runtime-registrable and remotable without touching the frozen filter chains, because traffic only reaches it by explicit RIB/policy selection.
2. **Provenance-chained peer (the BIBE-adjacent shape).** RIB policy forwards selected traffic to a scanner peer; re-injected traffic is distinguished by provenance (its `origin` records arrival from the scanner) so the second lookup does not re-select it. Caveat: a successful CLA forward is terminal today (report, delete, tombstone), so a *same-bundle* round-trip collides with dedup on re-entry; this shape needs the virtual-CLA **re-forward entry point** (non-terminal forward semantics) from the routing work.

**One bounded case stays inside the filter family: payload header peeking.** A Classifier may need the first few bytes of the payload — a wrapped IP header, an HTTP request line — purely to place a class: netfilter's `-m u32` shallow match, not its NFQUEUE. This is *bounded classification input*, not payload processing, and it fails none of the three tests above: there is no engine, no parking, and no verdict latency (a match on already-resident bytes); a ≤P-byte protocol-header decode is the same in-process risk class as the BPA's own CBOR parsing; and where BPSec encrypts the payload the peek reads ciphertext and classifies nothing — a deployment property the registering embedder knows. Mechanically it is free, because at every wire-facing hook the initial bytes of the bundle are memory-resident anyway. Registration declares the prefix per input-hook filter (the `_with_peek` registration variants; default 0), and `build()` fixes P as the maximum declared, so the zero-config node retains nothing; the ingress drain will keep min(P, payload length) payload bytes on the invocation side of the spool boundary when the streaming gate lands. Nothing is cached or persisted: the peek exists to stamp a class at classification time. The DPI line stays bright and *is* the bound: input bounded at `build()` → Classifier; unbounded or verdict-driven processing → component. Two inherent caveats: only an offset-0 fragment carries a meaningful prefix (others classify without it; the reassembled bundle re-crosses Ingest and is peeked properly), and a BCB-encrypted payload is the classifier's no-match path.

The refined criterion:

| Component | Target | Bytes | Mechanism |
|---|---|---|---|
| BIBE / tunnels | re-target | rewrites (encapsulates) | virtual CLA, RIB-selected |
| DPI / scanner | same-target | byte-pure, unbounded read | queue/verdict component (or provenance-chained peer) |
| Wrapped-header classification | same-target | byte-pure, first P bytes bounded at `build()` | ingress **Classifier** — payload header peeking |
| Extension-block edits | same-target | rewrites blocks, never payload | egress **Rewriter** — the pluggable slot in the rewrite stage |
| BPSec, per-hop built-ins | same-target | rewrites | fixed egress sequence — never pluggable |
| Compression, fragmentation, framing | same-target | rewrites payload/wire | CLA link adaptation |

The residual case externalisation cannot reach — post-decrypt plaintext at Deliver, the one place ciphertext becomes plaintext inside the BPA — is the receiving application platform's concern (or a wrapper service owning the endpoint), milliseconds before the app sees the bytes anyway. It does not justify a public trait.

## Registration: packs, frozen at `build()`

Filters ship in **`FilterPack`s** (`filter/pack`), the embedder's shipping unit reifying a filter pair's common construction code: a pack registers filters (per-hook methods, e.g. `ingress_classifier`, `egress_rewriter`), then `BpaBuilder::add_filters(pack)` splices it into the per-hook chains. The annotation slots a pair shares are declared, not registered (see [Annotation slots](#annotation-slots--embedder-private-metadata)). **Chain order is call order** — within a pack and across `add_filters` calls — lexically visible in the embedder's construction code. `build()` freezes each chain into a plain immutable slice the hot path iterates without synchronisation and fixes the payload-peek P; it has nothing to validate, so filter registration cannot fail it.

There is no registry object, no registration verb on a running BPA, and no name-keyed dependency resolution between filters: the component registries (CLA / Service / RoutingAgent / AdminRecord) are lock-guarded dynamic collections because components genuinely come and go at runtime; filters do not. Registered filters have no lifecycle — the BPA owns them from `add_filters` until shutdown, with no unregistration and no early teardown; a filter that needs its own lifecycle is a component. A `label` argument survives only as a diagnostic for logs and metrics, prefixed `"<pack>.<label>"` and never required to be unique.

Because a closed repository implements these traits without seeing `bpa` internals, the traits and the types they expose are **committed public extension API** — semver surface: the three kind traits, their contexts (`VerifyContext`, `ClassifyContext`, `RewriteContext`) and `Boundary`, `Verdict`, `MetadataDelta`/`Slot` (with the `slot!` macro)/`SlotValue`/`Blob`, `FilterPack`, and `bpa::bundle::BundleMetadata` itself with its read surface (see [What lives in bundle metadata](#what-lives-in-bundle-metadata)). The bpv7 types the trait signatures name — `Bundle`, `reader::Reader`, and `extension_editor::ExtensionEditor` with its `Error` — are bpv7's semver surface, versioned with that crate. Private group internals stay the BPA's to restructure.

## Alignment with the queue architecture

- Hooks land 1:1 in processing blocks (matrix above). Egress filters run in ClaSend, downstream of peer-seat FlowController scheduling.
- Status transitions become queue assignment (`enqueue` is the atomic commit point), so the metadata that remains is provenance and the wire cache plus filter-set classification: **filters annotate classification; the BPA owns lifecycle via queues.**
- Classification assigns the bundle's traffic class (see [`MetadataDelta` and the traffic class](#metadatadelta-and-the-traffic-class)); the FlowController's `push` places the bundle in its class queue, and each FlowController reads the class's properties from the frozen `ClassPolicy` — no translation anywhere. (This supersedes `queue_architecture.md`'s single-`flow_label` model — the queue doc's own principle survives in sharpened form: filters assign, the pipeline consumes. There is no filter-writable label field and no label parameter on the hot path — `FlowController::queue_for` takes none (the controller owns per-peer queue assignment); a traffic-class parameter for ECMP and HTB-style policies returns with the policy tranche, derived from classification.)
- At-least-once semantics make every hook chain re-runnable after a crash. Byte-pure verdicts and idempotent delta application satisfy this for free, and no rewrite-vs-enqueue crash window can arise, because no filter mutation is ever persisted: input hooks write only deltas, and Rewriter edits are per-attempt and in-memory.

## Restart re-admission

> **Phase 3 — settled design, not yet implemented.** The policy-epoch stamp already rides the classification group (engine bookkeeping, invisible to filters), and `clear_classification` exists as the clearing primitive, not yet called; the re-admission pass itself lands with the hook repositioning.

Because the chain is frozen in-process, filter policy can only change across a restart — a new binary, new construction wiring, or new config. Stored bundles were admitted and classified by the *previous* chain, so **restart recovery re-runs the input-hook chains over stored bundles**: new policy applies to traffic already in custody, not just to new arrivals.

What re-runs, precisely:

- **Input hooks only.** Egress and Deliver chains execute per transmission/delivery attempt, so they apply current policy naturally — and egress Rewriters are outside re-admission entirely: their edits are per-attempt and never persisted, so there is nothing stale to re-run. It is the input hooks whose effects persist — the admission verdict and the classification — and provenance picks the chain: ingress-entered bundles re-cross the Ingress chain, originated bundles the Originate chain.
- **Classification is re-derived from scratch.** Every persisted delta field is a cache of the chain's output, not an input to it; re-admission clears and re-derives them all, so removing a Classifier removes its annotations.
- **A Verifier drop at re-admission is a deletion in custody** — deletion report per the bundle's flags, never a fresh reception report (reception was reported once, at arrival). For originated bundles this is also the only notification path — the originator's `send` succeeded long ago.
- **The config-gated built-ins join the same pass.** The config is as restart-locked as the chain, so a tightened `primary_block_integrity` applies to stored bundles by the same rule — and those checks read the structural index, so they are equally metadata-only.
- **Fragments** re-cross the full Ingest processing via the reassembly path, unchanged — today once per fragment.

The payload-free kinds are what keep this affordable: re-admission never loads a payload. A Classifier that reads block bodies (or a payload peek) still gets them, because the engine supplies the invocation `data` by a **bounded head read** from `BundleStorage` — the persisted extents say how much is needed, and no new storage primitive is required: the engine calls the ordinary sequential `load` and drops its receiver once it has those bytes, which the backend observes and stops (`streaming_pipeline_design.md`). One bounded read per stale bundle, paid lazily: the seat is a **per-bundle stamp checked at the Dispatch block**, which every bundle already flows through. The stamp is a **policy epoch**, not a boot id: a restart bumps it, and so does a runtime class-policy push from a centralized policy manager (policy *data* flows at runtime through the component tier; only policy *code* rides the restart boundary) — the same mechanism re-derives classification in both cases, which an eager restart-time walk could never do for pushes. A filter's behaviour can change without its construction wiring changing shape (same registration code, new binary), so change detection is impossible — every restart conservatively re-admits everything. Accepted cost: a tightened Verifier purges a Waiting bundle only when a sweep next moves it; a background walk can close that gap if storage-pressure purging matters.

## `MetadataDelta` and the traffic class

The delta currently carries **annotation-slot writes only**; the two named fields arrive with their tranches:

| Field | What it is | Arrives with |
|---|---|---|
| `class: Option<ClassId>` | membership of a **traffic class** — the unit of differentiated treatment | policy tranche ([`policy_subsystem_redesign.md`](policy_subsystem_redesign.md)) |
| `route_key: Option<Eid>` | per-bundle routing key | routing tranche ([`routing_table_redesign.md`](routing_table_redesign.md)) |

**The class is the unit of policy.** A traffic class is defined once — in configuration, or by an embedder in Builder code — together with *all* of its per-dimension properties: dispatch weight, egress contracts, eviction rank, routing table. `build()` compiles the definitions into one frozen `ClassPolicy` table, and every consumer reads the bundle's class properties by field access on that shared, validated object. Two simpler shapes fail in opposite directions, and the class model is the fixed point between them:

- **A single opaque label as the unit of policy** puts an uninterpreted tag in metadata and a *separately configured* translation map at every consumption point — N config surfaces, N unmapped-value failure modes, and per-point config languages creeping toward a second rule system.
- **Per-dimension fields** (`priority`, `traffic_class`, `eviction_priority`, `route_table`) eliminate the maps but make the classifier explode one decision into many fields — and under scrutiny the fields keep collapsing into each other, because they are all the same class identity, differently named per consumer. A distinct treatment *is* a class; per-bundle values for enumerable dimensions add no expressive power.

One semantic identity in metadata, one definition owning all its properties, zero translation surfaces. This is also the correct reading of the tc precedent: iptables `-j CLASSIFY` sets `skb->priority` — a *class handle* whose meaning lives in the qdisc's class definitions, in one place — whereas the rejected shape is `-j MARK` plus per-device filter rules re-interpreting the mark at every qdisc.

**`route_key` remains a direct field because it fails the enumerability test** that justifies the class: an `Eid` (a label-stack top, a virtual class EID) is per-bundle data no finite class set can carry. That survives as the criterion for all delta growth: **enumerable treatment → class property; BPA-defined unbounded per-bundle data → delta field; embedder-defined data → annotation slot.** This also settles the marks / "trace mark" question: a would-be mark consumer registers a slot, and inter-filter signalling is the mechanism working as designed — no shared mark set exists, and there is no filter-writable label field. (The egress policy's flow-label *input* — ECMP hashing, HTB-style queue selection — remains in scope, fed from classification by the policy tranche.)

Mechanics: per-field last-writer-wins across the sequential Classifier chain; the classification group is serde-persisted and re-derived at restart re-admission — each persisted value is a cache of the chain's output, which also self-heals class-definition changes across a restart. With no classes configured every consumer sees the default class and the node behaves exactly as today — the zero-config baseline.

### Annotation slots — embedder-private metadata

Custom filter pairs — an ingress Classifier and an egress Rewriter shipped together — need to carry vendor-private intermediate state from admission to transmission: parse a proprietary extension block once at ingress, act on it at egress. The delta therefore supports **annotation slots** (`filter/slots`). A pair declares a slot with the `slot!` macro: a `static Slot<T>` whose at-rest name is its declaring path — the module path and the static's identifier. Rust's path namespace is the registry, so two declarations cannot share a name and nothing is registered or checked at run time. Classifiers write a slot through the delta (per-slot last-writer-wins, like every delta field); any filter that can name the static reads it. **Rust visibility is the access control**: a pair shares state by naming the same static, as built-ins use crate privacy. The scheme is cooperative, not cryptographic: trusted code could forge a `Slot` for any name. The BPA carries the values opaquely; the pair sees them fully typed. Two consequences of path naming: slots are declared at module scope (the name records the module, not the function, so same-named slots in two function bodies of one module would collide), and moving or renaming a declaration renames the slot, so values stored under the old name become unreadable — acceptable for a re-derivable cache. (Precedents: `http::Extensions` is Rust's middleware-pair pattern, but `TypeId` keys cannot persist; persistence needs a stable name, and a declaring path is one the compiler keeps unique.)

**The BPA checks nothing a Classifier writes**: filters are trusted, compiled-in code, so a runtime check of a filter's own limits would assert a property the code already fixes. Slot values are persisted with every bundle and held in memory with its record, so a Classifier that derives a value from sender-chosen data bounds it itself — by truncating, by hashing, or by storing a small key and re-reading the bytes at the hook that consumes it.

Slot values are coded with **hardy-cbor's `ToCbor`/`FromCbor`** (the `SlotValue` bound), not serde: no serde format crate exists in-tree, and the canonical codec gives LWW idempotence a byte-identity meaning for free. The byte-string asymmetry is on the encode side: hardy-cbor encodes `[u8]` with CBOR *array* semantics through its blanket slice impl and reserves byte-string encoding for the borrowing `encode::Bytes` wrapper, which cannot round-trip as a stored value — so `filter::slots::Blob` is the owned, two-way byte-string value (decoding through the codec's `Box<[u8]>` byte-string impl), and `BundleMetadata::slot_str`/`slot_bytes` read text and blob slots without copying — sound because `set()` encodes canonically, so a stored payload is always a definite-length contiguous item. Slots are **name-keyed at rest**: a value whose declaration moved or disappeared across a restart is unreadable, since no `Slot` names it, and goes when re-admission clears the classification group.

Slots inherit the classification group's semantics wholesale, which imposes the one contract: **a slot value is a cache of a pure derivation over (stored bytes, chain, config) — never a ledger.** Persisted with the bundle (an egress Rewriter may read it days after admission); cleared and re-derived at restart re-admission and policy-epoch bumps. Accumulating state, or anything that must travel between nodes, is not slot material: it belongs on the wire as an extension block, written by the Rewriter per transmission attempt, or in a shared inner (`Arc<Mutex<…>>`) the pair mints in its construction scope for cross-bundle node state. The node-scoped/network-scoped dual is unchanged — slots are the pair's node-scoped scratch; the Rewriter materialises the network-scoped result.

Built-ins get none of this: a built-in pair carries its node-scoped state (deferred verification results, NoKey watch state) in crate-private metadata fields directly, ordinary Rust privacy giving built-ins what a privately declared `Slot` gives embedders. That a built-in has no use for slots is the two-tier litmus test passing, not a gap.

## What lives in bundle metadata

The delta decision forces the wider question, and the answer is a principle: **metadata holds write-once facts, caches of pure derivations, and BPA infrastructure references — never independent mutable state.** Everything in it is either a historical fact or recomputable from (stored bytes, frozen chain, config), so nothing can be torn by a crash and the at-least-once story stays trivial.

| Group | Fields | Written by | Persisted | On restart | Filter visibility |
|---|---|---|---|---|---|
| **Provenance** | `received_at`; `origin: Ingress { peer_node, peer_addr, cla } \| Originated \| Recovered` | admission machinery, once | yes | kept — historical fact | read-only |
| **Wire cache** | `previous_node`, `age`, `hop_count` | parser, from the stored bytes | yes | kept — stored bytes never change | read-only |
| **Classification** | annotation slots (later `class`, `route_key`), plus the policy-epoch stamp | Classifier chain, via applied deltas | yes | cleared + re-derived (re-admission) | read; written only via the delta; slots gated by the declaring static; the epoch stamp invisible (engine bookkeeping) |
| **Infrastructure** | `storage_name` | BPA | yes | kept | **none — unreachable outside the crate** |

**Visibility is a property of each group**, as load-bearing as its writer, persistence, and restart fate — and it is enforced the way the Rewriter's payload-purity is: by construction, not code review. The per-kind contexts lend the record's own `BundleMetadata`, not a view of it: the record's own field/module privacy *is* the projection (private fields plus inline getters compile to field reads — the only zero-cost shape), so `bpa::bundle::BundleMetadata` is committed filter API directly, and there is no view type to drift from the record. Filters read provenance and the wire cache, read classification through the slot-gated accessors and write it only via the delta, and infrastructure does not exist in their world. Infrastructure stays BPA-private for safety in three senses — where "safety" means accident-prevention among trusted code, not defence: a mechanism reference in filter hands invites out-of-contract coupling; whatever the view exposes to a closed embedder is committed semver surface; and policy written against mechanism internals is meaningless policy — a rule keyed on a storage backend's naming scheme is not a rule about the bundle, and the shape makes such rules inexpressible rather than merely inadvisable.

Provenance is **persisted, write-once** (private fields + constructors, no `&mut` accessor): the `origin` enum records the arrival facts durably — the CLA name is a fact about arrival even if that CLA instance no longer exists — which makes the Egress transit predicate a type-level match and gives restart re-admission its chain selector. `Recovered` is the truthful origin for a bundle recovered from bundle storage without a metadata record, where fabricating an `Ingress` origin would be the exact lie provenance exists to kill.

Two things that look like metadata are deliberately not fields of the record — each is a fact about pipeline position:

- **`status`** is a field of `Bundle` itself, outside the metadata record and outside serde (backends encode it in their own typed columns) — the interim shape of "status is queue assignment", which the queue tranche completes.
- **`next_hop`** rides the **queue-assignment record**: `BundleStatus::ForwardPending { peer, queue, next_hop }`. The RIB lookup resolves the adjacency (`Rib::find` takes `&Bundle` immutably and returns it in the Forward action), the peer queue's send stamps it into the assignment, and `forward_bundle` extracts it from the status of the copy it claimed. It is persisted in the assignment record so the egress channels' at-least-once storage recovery re-delivers the *decision* intact — a transient field would forget the next hop across a restart or spill and mis-drive next-hop-dependent egress processing (the legacy re-encode). Recovery matches queue membership by queue *identity* (`BundleStatus::same_queue`, which ignores the per-bundle payload), while ownership swaps keep full-equality semantics. A queue-level constant was considered and rejected: a peer registers one NodeId per EID scheme, so there is no 1:1 peer→EID mapping. Egress Rewriters receive the resolved next hop as invocation context (their context's `Boundary::Egress`), not as metadata. When the queue tranche makes queues first-class, the assignment record generalises to the queue item (`ForwardItem { bundle, next_hop }` in the original sketch) — the shape anticipates that move.

Expiry remains a non-field — derived from the creation timestamp, lifetime, age, and (for unclocked sources) `received_at`, indexed at the storage layer for the reaper.

## Worked example — segment routing

Segment routing exercises the whole seam, licence-clean: an input Classifier derives the effective top of a label-stack extension block (skipping segments equal to self, so the RIB never sees key == self and deliver-vs-forward stays keyed on the real destination) and sets `route_key`; an egress Rewriter pops consumed segments onto the wire — the first known Rewriter consumer. Forward progress generalises from "re-targeting" to **"the lookup key is consumed"** ([`routing_table_redesign.md`](routing_table_redesign.md), which owns the RIB side: `route_key.unwrap_or(destination)`, tables, the FIB compilation). BPv7 block flags give strict vs best-effort waypoint semantics for free, and the label block joins the not-BIB-covered / `must_replicate` convention of the mutable per-hop family.

The per-attempt Rewriter model makes the pop restart-safe by construction: it commits only by transmission — the stored copy is never mutated — so no torn stack is ever persisted, at-least-once retransmissions re-derive byte-identical wire forms, and the frozen chain plus restart re-admission guarantee the classifying half and the popping half of the rule never skew. (Fragment residual-stack reassembly is a one-line rule for the eventual SR block spec, and a transitional one: IETF/CCSDS intend to deprecate RFC 9171 ADU fragmentation once BIBE standardises as its replacement.)

## Worked example — ESA's QoS extension block (UQEB)

Where segment routing exercises the Rewriter, ESA's QoS proposal (`draft-algarra-dtn-qos`, the User QoS Extension Block — source-added, transit-immutable, BIB-protected, carrying traffic priority, retransmission preference, latest-only delivery, and retention class) exercises the Classifier seam and the class model end-to-end.

The whole deployment is **one ingress/originate Classifier plus class definitions** — no new primitives. The Classifier reads the UQEB from the block index (the keyed header verify runs before the Ingress hook, so it sees a BIB-verified block — the draft's integrity MUST is satisfied by pipeline ordering) and maps `(source, requested parameters)` to a class. Because the draft's "user" is an SLA-contracted entity, the class set is the configured SLA profiles — a-priori configuration, safely enumerable despite the parameter cross-product — and clamping the requested tuple to the user's contracted class *is* the draft's Security Considerations policing ("ignore the requested handling" for unauthorized parameters): authorization and classification are the same operation. No Rewriter is involved: the UQEB is source-only and MUST NOT be modified in transit, so the one QoS block actually proposed for BPv7 never needs the mutation primitive.

Three of the four parameters are class properties consumed downstream, exactly per the criterion:

- **Traffic priority** → dispatch/peer-seat scheduling. The draft's strict per-user precedence differs from the open crate's weighted-fair default, and that is the policy split working: scheduling disciplines are FlowController implementations ([`policy_subsystem_redesign.md`](policy_subsystem_redesign.md)), so an ESA-conformant strict-priority-per-user controller is a registered scheduler type, not a BPA change.
- **Retransmission preference** → the class's routing `table` property: a table preferring reliable CLAs, "if possible" as fallback order, "required but unavailable" resolving to Wait.
- **Retention class** → the eviction-rank class property reserved for the storage-pressure tranche; the TTL tiebreak within a class is the storage layer's existing expiry index.

The fourth, **latest-only delivery, is deliberately not filter material** — and the design says so rather than bending. "Discard if a newer bundle from the same source to the same destination exists" is a cross-bundle query, and "the latest bundle is forwarded when the oldest discarded one would have been" is queue-position inheritance; a Classifier can express neither, because filter output is a pure derivation over one bundle's bytes and the arrival of a *newer* bundle must affect an *older* one already in custody. Latest-only is a queue discipline: a per-flow-replacement FlowController behaviour enabled by an enumerable class property, with flow identity computed at push. It is recorded as a named validation case against the `FlowController` trait shape in [`policy_subsystem_redesign.md`](policy_subsystem_redesign.md).

## Open items

- **Drop reporting from Originate/Deliver Verifiers.** Gate-pattern reporting is defined for Ingress; Phase 2 keeps today's per-hook behaviour (Originate returns the reason to the service; Deliver drops with reason or deletes). The formal semantics land with Phase 3's repositioning, alongside the reception-report reason-code fix ledgered in [`TODO.md`](TODO.md).
- **Scanner component.** Queue/verdict shape: a new registry row + queue wiring — how much lands with the queue-architecture work vs later; the provenance-chained shape waits on the virtual-CLA re-forward entry point in the routing work.
- **Fixed-vs-pluggable split for stripping the standard transport blocks at Deliver.** The RFC-defined blocks (Previous Node, Hop Count, Bundle Age) may be stripped by fixed machinery; the pluggable Deliver Rewriter targets embedder-defined transport blocks. The fixed strip is Phase 3 material.
- **Intra-chain classification reads.** Whether a later Classifier should read a predecessor's *pending* class assignment through the reader (it currently sees applied deltas, which is sufficient for slots); revisit when `class` arrives with the policy tranche.

## Roadmap

The working task list is [`refactor_plan.md`](refactor_plan.md). Phase 2 — the kinds, packs, slots, editor, engine swap, and dissolution of the built-in filters — is complete. Remaining:

- **Phase 3** — move the Ingress chain onto the pre-drain gate; formal Originate/Deliver drop semantics; restart re-admission of stored bundles. Early Drop then skips drain + store entirely: for a rejected 1 GB bundle the BPA has received only the header blocks (`streaming_pipeline_design.md` §5.4).
- **Phase 4** — scanner/verdict component and the virtual-CLA re-forward entry point. Waits on the queue-architecture and routing/RIB work; build when a consumer exists.

When the full streaming gate lands (`streaming_pipeline_design.md` §5.4/§5.7) the Ingress pass moves onto the accumulation buffer before any spool opens — the payload-free signatures make that a no-op for filter authors.

## Testing

- [Component Test Plan](component_test_plan.md) — pipeline-level integration coverage (`bpa/tests/filter_dispositions.rs` drives the Deliver row of the failure and Drop contract, the per-hop writes superseding an Egress Rewriter, and the editor's call-time refusals on attacker-chosen primaries; `bpa/tests/pipeline.rs` exercises the extent consistency from an Egress Rewriter through the per-hop writes; `bpa/tests/forward.rs` covers the legacy re-encode built-in, its drop of a bundle whose BPSec covers the primary by target, BIB scope or BCB scope, the bundles it takes when no operation does, and the Hop Count travelling unchanged under BPSec).
- [Unit Test Plan](unit_test_plan.md) — inline engine, editor, and slot tests.

## Related documents

- [`routing_table_redesign.md`](routing_table_redesign.md) — routing-key selection, the RIB→FIB compilation, multiple routing tables, inter-table jumps
- [`policy_subsystem_redesign.md`](policy_subsystem_redesign.md) — the `ClassPolicy` definition and properties, FlowControllers and scheduling, configuration, and the centralized policy manager
- [`queue_architecture.md`](queue_architecture.md) — processing blocks, queue assignment (its "Flow labels and classification" section is superseded by [`MetadataDelta` and the traffic class](#metadatadelta-and-the-traffic-class) above)
- [`streaming_pipeline_design.md`](streaming_pipeline_design.md) — §5.2.2 (reject, don't rewrite), §5.3–5.7 (the gate, tee'd ingress), §6.1 (egress seam)
- [`policy_subsystem_design.md`](policy_subsystem_design.md) — the current policy design (to be replaced by the policy redesign)
- [`../../docs/acme_design.md`](../../docs/acme_design.md) — the AdminRecord registry (admin-path extension surface)
