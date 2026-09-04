# BPA Filter Subsystem Design

The embedder's extension seam on the bundle pipeline: three payload-free filter kinds, registered in construction-frozen packs, run inline at four hook points.

> **Status.** This document describes the implemented registration surface, filter kinds, and engine (the filter redesign's Phase 2, per [`refactor_plan.md`](refactor_plan.md)). The Ingress chain runs at the pre-drain gate (the Phase-3 repositioning landed with the ingress-spool tranche). Two settled parts of the design are not yet implemented and are flagged where they appear: [restart re-admission](#restart-re-admission) (Phase 3), and the [scanner component](#payload-inspection-is-a-component-not-a-filter-kind) (Phase 4). The `class`/`route_key` classification fields arrive with the policy and routing tranches ([`policy_subsystem_redesign.md`](policy_subsystem_redesign.md), [`routing_table_redesign.md`](routing_table_redesign.md)).

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

The evidence base for the taxonomy. Every processing point in the in→out pipeline, classified by (a) read or write, (b) externally pluggable. Stage names follow the processing blocks of [`queue_architecture.md`](queue_architecture.md); function references are current `dispatcher/` code. All four ★ hook rows are at their designed positions (the Ingress chain runs at the pre-drain gate, beside the config-gated built-ins).

**In from a peer** (Ingest block — `receive_bundle` → `process_received_bundle`):

| Processing point | R/W | Pluggable? |
|---|---|---|
| CLA deframing → segment stream (`stream::Segment` via `Sink::dispatch`) | write (assemble) | already — CLA trait |
| Structural header parse, canonical enforcement (`parse_headers`) | read (reject) | no — spec |
| Keyed header verify: BIB verify, BCB decrypt for extraction, NoKey liveness | read (reject) | keys already via KeyProvider/KeySource |
| Extension fields → metadata wire cache | write (meta) | no |
| Pre-drain gate: lifetime / hop exhaustion (`gate_reason`) + the config-gated RFC 9171 checks (`rfc9171_gate_reason`) | read (reject) | no — spec/config |
| **★ Ingress hook** — registered Verifiers ∥, then Classifiers | read + annotate (delta) | **yes — the hook** (headers + metadata, no payload) |
| Payload drain/spool through `ValidatingReceiver` (payload CRC, breaks, deferred block-1 BIB digests) | write (accumulate), read (reject) | no — parser/BPSec owns |
| §5.1.1 failure-drops + unrecognised-block removals — *scheduled* in `to_remove` metadata, applied per attempt at the output doors; stored bytes stay as-received | write (meta) | no |
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
| Per-hop rewrite: PreviousNode insert, HopCount increment, BundleAge update, and the config-driven IPN legacy re-encode (`update_extension_blocks`) | **write (extension blocks)** | no — spec §4.4.x + config; the fixed head of the rewrite stage |
| **★ Egress hook (rewrite)** — registered Rewriters, seq | **write (extension blocks)** | **yes — the hook** (in-memory, per transmission attempt; payload/primary/BIB/BCB excluded by the editor handle) |
| **★ Egress hook (gate)** — registered Verifiers ∥ | read (reject) | **yes — the hook** (gates the final pre-BPSec wire form; re-runs with fresh context on re-route) |
| BPSec-egress seam | **write (bytes)** | no — fixed, security-policy/KeyProvider-driven |
| CLA: fragmentation + framing | write | already — CLA trait |

**Out to a local application** (Deliver block):

| Processing point | R/W | Pluggable? |
|---|---|---|
| Load from store | — | no |
| **★ Deliver hook** — registered Rewriters (seq, transport-block strip), then Verifiers ∥ | read (reject) + **write (extension blocks)** | **yes — the hook** (raw-`Service` path only; before payload decrypt) |
| BCB payload decrypt — produces the ADU | write (plaintext) | no — normal BPA functionality |
| `service.on_deliver` | — | already — Service trait |
| Delivery report; delete | write | no — spec |

Two findings carry the whole design:

- **Payload and primary-block writes are exclusively fixed, spec-owned machinery** — the parser at ingress, BPSec at egress, decrypt at deliver, framing in the CLA; their ordering is RFC-mandated and none is a plausible external plug point. The one pluggable mutation point is the extension-block rewrite at ClaSend — where the pipeline already edits blocks per hop — and it is pluggable *by scope* (extension blocks only, via the Rewriter's editor handle), not by exception.
- **Every input-side hook is read + annotate.** Inputs never mutate wire bytes; the mutating hook capability exists only on the output side, where each hop's wire form is derived.

## The filter kinds

Three kinds, defined in `bpa/src/filter/mod.rs` (`Verifier`, `Classifier`, `Rewriter`; rustdoc owns the signatures). Every invocation reads the bundle through a `BundleReader`, which bundles into one borrow: the `Bundle` (primary block, structural block index, and the BPA metadata — provenance, wire cache, classification), the resident source bytes, the decoded BCB OperationSets, and a `KeySource` resolved through the existing KeyProvider seam. Block bodies come back plaintext-or-decrypted through `block_data`/`extract`; a BCB-covered block the node holds no key for is the filter's no-match path (`Ok(None)`), never an error it must handle. All three kinds return the shared `Verdict` enum, so the drop path — and its status-report reason — is identical everywhere.

| Kind | May do | Mutates | Execution | Hooks |
|---|---|---|---|---|
| **Verifier** | `Continue` / `Drop(reason)` | nothing | parallel (independence contract) | all |
| **Classifier** | `Continue(MetadataDelta)` / `Drop(reason)` | metadata, via delta | sequential | inputs only |
| **Rewriter** | edit extension blocks / `Drop(reason)` | extension blocks, via a scoped editor handle | sequential | **Egress + Deliver** |

**The mutation boundary is the block, not "bytes".** Extension blocks are BPv7's own extensibility surface — where the protocol itself says new wire-visible behaviour lives — so a filter system whose purpose is licence-clean custom processing rules must be able to add, modify, and remove them. What filters can never touch: the **payload block** (the application's data, owned end-to-end by the application, BPSec, and the CLA), the **primary block** (immutable per RFC 9171), and **security blocks** (BIB/BCB — the BPSec seams' monopoly). The Rewriter's `ScopedEditor` handle exposes exactly the permitted operations — `insert`/`replace`/`remove` of extension blocks — making payload-purity a compile-time property rather than a code-review promise. It refuses edits to blocks under existing BPSec coverage (this node is not their security source), conservatively including blocks whose coverage is unprovable because a BIB is undecryptable; and it refuses a target block number the bundle does not have, where the underlying bpv7 editor would silently no-op — a scoped-policy layer fails loudly. Operations are validated as they are called, so a refusal is the Rewriter's own no-match path; the engine materialises the accumulated edits only when the invocation returns `Continue`.

The two mutating kinds are duals by scope: the **Classifier** (inputs) writes *node-scoped* annotations — metadata this node's own downstream consumes; the **Rewriter** (egress) writes *network-scoped* annotations — extension blocks the next hops consume. A rule whose effect is local needs a delta; a rule whose effect must travel needs a block.

All three kinds are payload-free and therefore **streaming-immune**: at ingress, Verifiers and Classifiers need only the header blocks (plus any declared payload peek) and ride the pre-drain gate unchanged (`streaming_pipeline_design.md` §5.4; the §5.7 tee'd spool is still to come); the Rewriter edits header blocks, which are resident at ClaSend where the per-hop rewrite already operates, while the payload streams past untouched. There is no late ingress pass — an early/late Ingress split would exist only to serve in-pipeline payload inspection, and with none there is nothing a late pass could see that the pre-drain pass cannot.

The Classifier returns a *delta* rather than taking `&mut metadata` deliberately: the engine applies it, filters never touch `bundle.metadata` directly, the boundary stays clean for closed-source implementors, and delta application is idempotent — which the queue architecture's at-least-once semantics require of every processing block. A Classifier **sees the deltas applied by preceding links** of the same pass: the engine applies each delta before the next invocation.

The Rewriter's execution model is **in-memory, per transmission attempt** — exactly the semantics of the built-in per-hop rewrite it extends. The stored bundle remains as received (post-parser canonicalisation); each hop's wire form is derived fresh at ClaSend and never written back. That single choice is what keeps the rest of the design intact: no persisted filter mutations means no crash window between rewrite and enqueue, restart re-admission stays metadata-only, and a re-routed bundle is re-prepared for its new peer with fresh context.

**Filters are trusted code.** No view type mediates byte access, because filters are in-process, link-time, supplied by the embedder — the registration seam is a *licence* boundary, not a *privilege* boundary, and there is no security boundary inside a process for a wrapper type to enforce. What is real is the **residence contract**: every invocation sees the header blocks plus the first P declared peek bytes, while payload residence beyond P is never guaranteed (hook position, streaming state). The reader's optional returns express the variability uniformly. A filter that *depends* on payload bytes has written itself out of streaming-immunity, and the design's answer to payload-hungry processing remains the component tier ([below](#payload-inspection-is-a-component-not-a-filter-kind)).

## Hooks and the matrix

| Hook | Processing block | Position | Verifier | Classifier | Rewriter |
|---|---|---|:--:|:--:|:--:|
| **Ingress** | Ingest | pre-drain, pre-store (at the gate) | ✓ | ✓ | — |
| **Originate** | Originate | pre-store, in-memory | ✓ | ✓ | — |
| **Egress** | ClaSend | after per-hop rewrite, before BPSec | ✓ | — | ✓ |
| **Deliver** | Deliver | before payload decrypt | ✓ | — | ✓ (transport-block strip) |

**Classifier is inputs-only.** Classification annotates metadata that the node's *own downstream* consumes — the traffic class whose properties drive dispatch weighting, egress contracts, eviction, and table selection, plus the routing key and any registered annotation slots. There is no downstream inside the BPA past an output boundary, so an Egress or Deliver Classifier has nothing to feed.

**Rewriter is egress + deliver — the two output boundaries.** Inputs never rewrite — incoming wire bytes are canonical truth, rejected at parse, never rewritten (`streaming_pipeline_design.md` §5.2.2) — and a locally-originated bundle's blocks are the Builder/service tier's to assemble. Wire preparation for the next hop belongs at the one point with next-hop context, per transmission attempt; that also covers locally-originated traffic, since Originate→Egress passes through ClaSend like everything else. Deliver is the *dual*: it strips **transport-scoped** extension blocks — network QoS, custody, the per-hop plumbing ("transport headers", to borrow HTTP's hop-by-hop/end-to-end split) — so a terminating bundle hands *content* to the application, not network bookkeeping (a security property too: Previous Node and internal QoS policy do not leak to an untrusted app). This applies only to the raw-bundle `Service` path; the payload-only `Application` path already sees no extension blocks. Because the Rewriter edits extension blocks and never the payload, the Deliver invocation sits *before* the payload's BPSec decrypt and uses its `KeySource` to decrypt any extension block it must inspect. Next-hop context is Egress-only (a delivering bundle has no next hop), so it rides the `RewriteContext::Egress` variant rather than the method signature.

**There is no FORWARD hook.** The pipeline has exactly one out-to-peer processing point — locally-originated and transit traffic converge before ClaSend — so the output hook is netfilter's POSTROUTING, not its FORWARD. Netfilter needs a separate FORWARD chain because flat rule tables cannot ask "is this transit?"; our filters are code with metadata access, and the persisted provenance (`origin` is `Ingress`) at Egress *is* the transit predicate. The two things a FORWARD hook would add beyond that predicate have better homes: acting once per routing decision is an Ingress matter (provenance never changes after ingest), and changing where a bundle goes is the RoutingAgent's job — a filter that redirects is a routing agent in a costume. The hook keeps the name **Egress** deliberately: it does *not* have netfilter-FORWARD semantics (it sees originated traffic too), and naming it Forward would import the transit-only intuition.

**The admin path is hook-free by design.** Admin records addressed to this node terminate in the Admin block, whose extension surface is the **AdminRecord registry** (registry + sink keyed by record type, with record-type-claim authorisation at the gRPC layer — `docs/acme_design.md` §4). Filters never intercept the control plane's terminal processing; admin bundles still cross Ingress like any other bundle, so boundary policing applies.

## Execution: the inline engine

The frozen chains run **synchronously, inline on the calling task** (`filter/engine.rs`). Two facts force this and one payoff justifies it:

- Filter invocations are synchronous by trait signature — async methods would exist only to serve callout-style filters (gRPC policy engines, database lookups), which the two-tier split places in the component tier; policy-as-code on the hot path has no await points.
- The decoded BCB OperationSets the `BundleReader` lends are not `Send`, so an invocation cannot migrate across tasks anyway.

"Parallel" Verifiers is therefore an **independence contract** — no ordering, no cross-talk between the Verifiers of one hook — not a spawning strategy, so the engine needs no task pool and no pool-deadlock reasoning. An empty chain costs one branch: nothing is parsed, nothing is allocated, so hooks nobody extends are near-free.

Two engine behaviours are load-bearing for the dispatcher's claim discipline:

- **Every runner returns the bundle to the caller on both the verdict and the error path** (`ChainOutcome`, and errors as `(Bundle, error)`). The output hooks run inside a claimed window (`ForwardAckPending`/`DeliveryAckPending`), and the site that claimed the bundle resolves the claim with the same record on every exit — no re-fetch, no restore path, conditional swaps stay clean.
- **Each Rewriter sees its predecessors' edits**: the engine materialises every invocation's accepted edits into the wire form (and re-indexes the block map) before the next invocation reads it — the mutation-side mirror of the Classifier delta rule.

## Where bytes change

**Payload and primary-block mutation is exclusively fixed machinery**; extension-block mutation has exactly one pluggable point, inside the egress sequence:

```
load from store                          (stored bundle = as received; source of truth)
  └▶ built-in per-hop rewrite            PreviousNode / HopCount / BundleAge — fixed, spec §4.4.x
                                         + the config-driven IPN legacy re-encode (ipn_legacy_peers)
  └▶ registered Rewriters (seq)          extension-block add / modify / remove — THE pluggable mutation point
  └▶ Egress Verifiers (∥)                gate the final pre-BPSec wire form
  └▶ BPSec-egress seam                   add BIB/BCB per security policy — fixed, KeyProvider-driven
  └▶ CLA                                 fragmentation + framing — fixed link adaptation
```

The Rewriters sit where they do for two reasons: after the built-in per-hop rewrite so they act on the near-final form (and are, semantically, an extension of that stage — the built-ins are simply the fixed head of it), and before the BPSec seam so the blocks they add or modify can be signed per security policy. Verifiers run after the Rewriters so they gate what actually ships.

The IPN legacy re-encode (3-element `ipn` EIDs re-encoded 2-element for peers that require the older form) is a built-in of this stage rather than a registered filter, by this design's own taxonomy: it rewrites the *primary block*, which is out of every Rewriter's scope by construction, and it is per-hop wire adaptation, not embedder policy. It is driven by `BpaBuilder::ipn_legacy_peers` (EID patterns matched against the resolved next hop) and — like every rewrite in this stage — applies only to the transmitted wire form: the record's own primary is never replaced, so id-keyed resolution after transmission (tombstones, dedup) always compares like with like.

The fixed byte-owners elsewhere are unchanged:

- **Ingress**: the parser — canonical rejection (never rewrite incoming wire bytes), BPSec verify/decrypt, the RFC 9172 §5.1.1 removal cascade.
- **Deliver**: BCB payload decrypt — normal BPA functionality.
- **Re-targeting** (BIBE, tunnelling, overlays): RIB-selected **virtual CLAs**. The carrier gets a new destination, so the next lookup makes forward progress; chaining is the sequence of carrier destinations; loop protection is ordinary hop-count/age.

There is no payload-rewriting egress Transform. The slot such a filter would reserve belongs to BPSec (a fixed seam), and its other tenants are payload operations with better homes: compression and framing are the CLA's, transcoding/redaction are rewriting gateways (components), aggregation is application-layer or BIBE-shaped encapsulation. The Rewriter is not a Transform — it is scoped to extension blocks, extends a stage that already mutates them, and its output remains subject to the fixed BPSec seam behind it.

## BPSec roles at the boundaries

The fixed BPSec machinery named above follows the RFC 9172 §3.2 role model, mapped onto the same boundaries the hooks sit at. bpv7 is policy-free mechanism (*check* functions decrypt/verify and report; *edit* functions remove/replace blocks); bpa owns policy and call sites. A node holding a key is **at least a Verifier**: it validates, keeps valid blocks encrypted, and applies the §5.1.1 failure-drop (removing corrupt ciphertext only, exposing no plaintext). An **Acceptor** additionally *consumes* a valid operation (decrypt + strip), deferred to Deliver/Egress so storage stays encrypted; the waypoint-acceptor call site is the fixed BPSec-egress seam in ClaSend — built-in pipeline, never a registered filter.

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

Filters ship in **`FilterPack`s** (`filter/pack`), the embedder's shipping unit reifying a filter pair's common construction code: a pack registers annotation slots (`annotation_slot`, returning the typed `SlotHandle` both halves of a pair share) and filters (per-hook methods, e.g. `ingress_classifier`, `egress_rewriter`), then `BpaBuilder::add_filters(pack)` splices it into the per-hook chains. **Chain order is call order** — within a pack and across `add_filters` calls — lexically visible in the embedder's construction code. `build()` validates pack names, freezes each chain into a plain immutable slice the hot path iterates without synchronisation, freezes the slot table, and fixes the payload-peek P.

There is no registry object, no registration verb on a running BPA, and no name-keyed dependency resolution between filters: the component registries (CLA / Service / RoutingAgent / AdminRecord) are lock-guarded dynamic collections because components genuinely come and go at runtime; filters do not. Registered filters have no lifecycle — the BPA owns them from `add_filters` until shutdown, with no unregistration and no early teardown; a filter that needs its own lifecycle is a component. A `label` argument survives only as a diagnostic for logs and metrics, prefixed `"<pack>.<label>"` and never required to be unique.

The pack name also prefixes slot names at rest (`"<pack>.<slot>"`), which makes cross-pack slot collisions unrepresentable: `build()`'s `DuplicateName` fires only within a pack (or for two same-named packs — two writers who should be sharing a handle, not a name).

Because a closed repository implements these traits without seeing `bpa` internals, the traits and the types they expose are **committed public extension API** — semver surface: the three kind traits, `Verdict`, `BundleReader`, `RewriteContext`, `ScopedEditor` and its operation set, `MetadataDelta`/`SlotHandle`/`SlotValue`/`Blob`, `FilterPack`, and `bpa::bundle::Bundle` itself with the metadata read surface (see [What lives in bundle metadata](#what-lives-in-bundle-metadata)). Private group internals stay the BPA's to restructure.

## Alignment with the queue architecture

- Hooks land 1:1 in processing blocks (matrix above). Egress filters run in ClaSend, downstream of peer-seat FlowController scheduling.
- Status transitions become queue assignment (`enqueue` is the atomic commit point), so the metadata that remains is provenance and the wire cache plus filter-set classification: **filters annotate classification; the BPA owns lifecycle via queues.**
- Classification assigns the bundle's traffic class (see [`MetadataDelta` and the traffic class](#metadatadelta-and-the-traffic-class)); the FlowController's `push` places the bundle in its class queue, and each FlowController reads the class's properties from the frozen `ClassPolicy` — no translation anywhere. (This supersedes `queue_architecture.md`'s single-`flow_label` model — the queue doc's own principle survives in sharpened form: filters assign, the pipeline consumes. There is no filter-writable label field; the flow label as the egress policy's *input* remains in scope — `EgressPolicy::classify` keeps its label parameter for ECMP and HTB-style policies, invoked with `None` until the policy tranche derives the label from classification.)
- At-least-once semantics make every hook chain re-runnable after a crash. Byte-pure verdicts and idempotent delta application satisfy this for free, and no rewrite-vs-enqueue crash window can arise, because no filter mutation is ever persisted: input hooks write only deltas, and Rewriter edits are per-attempt and in-memory.

## Restart re-admission

> **Phase 3 — settled design, not yet implemented.** The policy-epoch stamp already rides the classification group (engine bookkeeping, invisible to filters), and `clear_classification` is the wired clearing primitive; the re-admission pass itself lands with the hook repositioning.

Because the chain is frozen in-process, filter policy can only change across a restart — a new binary, new construction wiring, or new config. Stored bundles were admitted and classified by the *previous* chain, so **restart recovery re-runs the input-hook chains over stored bundles**: new policy applies to traffic already in custody, not just to new arrivals.

What re-runs, precisely:

- **Input hooks only.** Egress and Deliver chains execute per transmission/delivery attempt, so they apply current policy naturally — and egress Rewriters are outside re-admission entirely: their edits are per-attempt and never persisted, so there is nothing stale to re-run. It is the input hooks whose effects persist — the admission verdict and the classification — and provenance picks the chain: ingress-entered bundles re-cross the Ingress chain, originated bundles the Originate chain.
- **Classification is re-derived from scratch.** Every persisted delta field is a cache of the chain's output, not an input to it; re-admission clears and re-derives them all, so removing a Classifier removes its annotations.
- **A Verifier drop at re-admission is a deletion in custody** — deletion report per the bundle's flags, never a fresh reception report (reception was reported once, at arrival). For originated bundles this is also the only notification path — the originator's `send` succeeded long ago.
- **The config-gated built-ins join the same pass.** The config is as restart-locked as the chain, so a tightened `primary_block_integrity` applies to stored bundles by the same rule — and those checks read the structural index, so they are equally metadata-only.
- **Fragments** re-cross the full Ingest processing via the reassembly path, unchanged.

The payload-free kinds are what keep this affordable: re-admission never loads a payload. A Classifier that reads block bodies (or a payload peek) still gets them, because the engine supplies the invocation `data` by a **bounded head read** from `BundleStorage` — the persisted extents say how much is needed, and no new storage primitive is required: the engine calls the ordinary sequential `load` and drops its receiver once it has those bytes, which the backend observes and stops (`streaming_pipeline_design.md`). One bounded read per stale bundle, paid lazily: the seat is a **per-bundle stamp checked at the Dispatch block**, which every bundle already flows through. The stamp is a **policy epoch**, not a boot id: a restart bumps it, and so does a runtime class-policy push from a centralized policy manager (policy *data* flows at runtime through the component tier; only policy *code* rides the restart boundary) — the same mechanism re-derives classification in both cases, which an eager restart-time walk could never do for pushes. A filter's behaviour can change without its construction wiring changing shape (same registration code, new binary), so change detection is impossible — every restart conservatively re-admits everything. Accepted cost: a tightened Verifier purges a Waiting bundle only when a sweep next moves it; a background walk can close that gap if storage-pressure purging matters.

## `MetadataDelta` and the traffic class

The delta is `#[non_exhaustive]` and currently carries **annotation-slot writes only**; the two named fields arrive additively with their tranches:

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

Custom filter pairs — an ingress Classifier and an egress Rewriter shipped together — need to carry vendor-private intermediate state from admission to transmission: parse a proprietary extension block once at ingress, act on it at egress. The delta therefore supports **registered annotation slots** (`filter/slots`): the pack registers a slot (stable name + value type + size bound) and receives a typed `SlotHandle<T>`; `build()` rejects name collisions loudly, and the per-slot size bound at delta application keeps metadata stores honest (an over-sized write is dropped with a warning). Classifiers write a slot through the delta (per-slot last-writer-wins, like every delta field); any filter holding the handle reads it. **The handle is the capability**: possession grants access, so a pair shares state by sharing the handle in its common construction code — privacy by unforgeability, no permission machinery. The BPA carries the values opaquely; the pair sees them fully typed. (Precedents: `http::Extensions` is Rust's middleware-pair pattern, but `TypeId` keys cannot persist; DPDK's registered mbuf dynfields supply the registration-at-init model; persistence forces the combination — stable registered names outside, typed handles inside.)

Slot values are coded with **hardy-cbor's `ToCbor`/`FromCbor`** (the `SlotValue` bound), not serde: no serde format crate exists in-tree, and the canonical codec gives LWW idempotence a byte-identity meaning for free. The codec's zero-copy stance carries through — `FromCbor` deliberately has no owned `String`/`Box<[u8]>` container decodes hidden in stock impls, so `filter::slots::Blob` is the explicit owned byte-string value (bare byte containers would encode with CBOR *array* semantics through the blanket slice impl), and `BundleMetadata::slot_str`/`slot_bytes` read text and blob slots without copying — sound because `set()` encodes canonically, so a stored payload is always a definite-length contiguous item. Slots are **name-keyed at rest**: a load meeting an unregistered name (the embedder changed across a restart) is unreadable without a handle and is pruned harmlessly at re-admission.

Slots inherit the classification group's semantics wholesale, which imposes the one contract: **a slot value is a cache of a pure derivation over (stored bytes, chain, config) — never a ledger.** Persisted with the bundle (an egress Rewriter may read it days after admission); cleared and re-derived at restart re-admission and policy-epoch bumps. Accumulating state, or anything that must travel between nodes, is not slot material: it belongs on the wire as an extension block, written by the Rewriter per transmission attempt, or in a shared inner (`Arc<Mutex<…>>`) the pair mints in its construction scope for cross-bundle node state. The node-scoped/network-scoped dual is unchanged — slots are the pair's node-scoped scratch; the Rewriter materialises the network-scoped result.

Built-ins get none of this: a built-in pair carries its node-scoped state (deferred verification results, NoKey watch state) in crate-private metadata fields directly, ordinary Rust privacy giving built-ins what the unforgeable handle gives embedders. That a built-in has no use for slots is the two-tier litmus test passing, not a gap.

## What lives in bundle metadata

The delta decision forces the wider question, and the answer is a principle: **metadata holds write-once facts, caches of pure derivations, and BPA infrastructure references — never independent mutable state.** Everything in it is either a historical fact or recomputable from (stored bytes, frozen chain, config), so nothing can be torn by a crash and the at-least-once story stays trivial.

| Group | Fields | Written by | Persisted | On restart | Filter visibility |
|---|---|---|---|---|---|
| **Provenance** | `received_at`; `origin: Ingress { peer_node, peer_addr, cla } \| Originated \| Recovered` | admission machinery, once | yes | kept — historical fact | read-only |
| **Wire cache** | `previous_node`, `age`, `hop_count` | parser, from the stored bytes | yes | kept — stored bytes never change | read-only |
| **Classification** | registered annotation slots (later `class`, `route_key`), plus the policy-epoch stamp | Classifier chain, via applied deltas | yes | cleared + re-derived (re-admission) | read; written only via the delta; slots gated per-handle; the epoch stamp invisible (engine bookkeeping) |
| **Infrastructure** | `storage_name` | BPA | yes | kept | **none — unreachable outside the crate** |

**Visibility is a property of each group**, as load-bearing as its writer, persistence, and restart fate — and it is enforced the way the Rewriter's payload-purity is: by construction, not code review. There is no per-invocation view struct: the record's own field/module privacy *is* the projection (private fields plus inline getters compile to field reads — the only zero-cost shape), so `bpa::bundle::Bundle` is committed filter API directly, and there is no view type to drift from the record. Filters read provenance and the wire cache, read classification through the handle-gated accessors and write it only via the delta, and infrastructure does not exist in their world. Infrastructure stays BPA-private for safety in three senses — where "safety" means accident-prevention among trusted code, not defence: a mechanism reference in filter hands invites out-of-contract coupling; whatever the view exposes to a closed embedder is committed semver surface; and policy written against mechanism internals is meaningless policy — a rule keyed on a storage backend's naming scheme is not a rule about the bundle, and the shape makes such rules inexpressible rather than merely inadvisable.

Provenance is **persisted, write-once** (private fields + constructors, no `&mut` accessor): the `origin` enum records the arrival facts durably — the CLA name is a fact about arrival even if that CLA instance no longer exists — which makes the Egress transit predicate a type-level match and gives restart re-admission its chain selector. `Recovered` is the truthful origin for a bundle recovered from bundle storage without a metadata record, where fabricating an `Ingress` origin would be the exact lie provenance exists to kill.

Two things that look like metadata are deliberately not fields of the record — each is a fact about pipeline position:

- **`status`** is a field of `Bundle` itself, outside the metadata record and outside serde (backends encode it in their own typed columns) — the interim shape of "status is queue assignment", which the queue tranche completes.
- **`next_hop`** rides the **queue-assignment record**: `BundleStatus::ForwardPending { peer, queue, next_hop }`. The RIB lookup resolves the adjacency (`Rib::find` takes `&Bundle` immutably and returns it in the Forward action), the peer queue's send stamps it into the assignment, and `forward_bundle` extracts it from the status of the copy it claimed. It is persisted in the assignment record so the egress channels' at-least-once storage recovery re-delivers the *decision* intact — a transient field would forget the next hop across a restart or spill and mis-drive next-hop-dependent egress processing (the legacy re-encode). Recovery matches queue membership by queue *identity* (`BundleStatus::same_queue`, which ignores the per-bundle payload), while ownership swaps keep full-equality semantics. A queue-level constant was considered and rejected: a peer registers one NodeId per EID scheme, so there is no 1:1 peer→EID mapping. Egress filters receive the resolved next hop as invocation context (`RewriteContext::Egress`), not as metadata. When the queue tranche makes queues first-class, the assignment record generalises to the queue item (`ForwardItem { bundle, next_hop }` in the original sketch) — the shape anticipates that move.

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

- **Drop reporting from Originate/Egress/Deliver Verifiers.** Gate-pattern reporting is defined (and implemented) for Ingress; the other hooks keep their per-hook behaviour (Originate returns the reason to the service; Egress/Deliver drop with reason or delete). The formal semantics land with the rest of Phase 3, alongside the reception-report reason-code fix ledgered in [`TODO.md`](TODO.md).
- **Scanner component.** Queue/verdict shape: a new registry row + queue wiring — how much lands with the queue-architecture work vs later; the provenance-chained shape waits on the virtual-CLA re-forward entry point in the routing work.
- **Fixed-vs-pluggable split for stripping the standard transport blocks at Deliver.** The RFC-defined blocks (Previous Node, Hop Count, Bundle Age) may be stripped by fixed machinery; the pluggable Deliver Rewriter targets embedder-defined transport blocks. The fixed strip is Phase 3 material.
- **Intra-chain classification reads.** Whether a later Classifier should read a predecessor's *pending* class assignment through the reader (it currently sees applied deltas, which is sufficient for slots); revisit when `class` arrives with the policy tranche.

## Roadmap

The working task list is [`refactor_plan.md`](refactor_plan.md). Phase 2 — the kinds, packs, slots, editor, engine swap, and dissolution of the built-in filters — is complete. Remaining:

- **Phase 3 (remaining)** — formal Originate/Egress/Deliver drop semantics; restart re-admission of stored bundles. The Ingress-chain move onto the pre-drain gate landed with the ingress-spool tranche: an early Drop skips drain + store entirely, so for a rejected 1 GB bundle the BPA has received only the header blocks (`streaming_pipeline_design.md` §5.4).
- **Phase 4** — scanner/verdict component and the virtual-CLA re-forward entry point. Waits on the queue-architecture and routing/RIB work; build when a consumer exists.

The Ingress pass runs on the accumulation buffer before any spool opens — the payload-free signatures made that a no-op for filter authors, as designed.

## Testing

- [Component Test Plan](component_test_plan.md) — pipeline-level integration coverage (`bpa/tests/pipeline.rs` exercises registered-filter drops and the extent-consistency Egress Verifier; `bpa/tests/slots.rs` covers pack registration and slot round-trips; `bpa/tests/forward.rs` covers the legacy re-encode built-in).
- [Unit Test Plan](unit_test_plan.md) — inline engine, editor, and slot-state tests.

## Related documents

- [`routing_table_redesign.md`](routing_table_redesign.md) — routing-key selection, the RIB→FIB compilation, multiple routing tables, inter-table jumps
- [`policy_subsystem_redesign.md`](policy_subsystem_redesign.md) — the `ClassPolicy` definition and properties, FlowControllers and scheduling, configuration, and the centralized policy manager
- [`queue_architecture.md`](queue_architecture.md) — processing blocks, queue assignment (its "Flow labels and classification" section is superseded by [`MetadataDelta` and the traffic class](#metadatadelta-and-the-traffic-class) above)
- [`streaming_pipeline_design.md`](streaming_pipeline_design.md) — §5.2.2 (reject, don't rewrite), §5.3–5.7 (the gate, tee'd ingress), §6.1 (egress seam)
- [`policy_subsystem_design.md`](policy_subsystem_design.md) — the current policy design (to be replaced by the policy redesign)
- [`../../docs/acme_design.md`](../../docs/acme_design.md) — the AdminRecord registry (admin-path extension surface)
