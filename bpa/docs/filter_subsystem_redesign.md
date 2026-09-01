# Filter Subsystem Redesign (DRAFT / RFC)

> **Status: draft for review; partially implemented.** Replaces the current `ReadFilter`/`WriteFilter` model (modelled on netfilter's filter/mangle tables); the design aligns with [`queue_architecture.md`](queue_architecture.md). Once accepted this folds into [`filter_subsystem_design.md`](filter_subsystem_design.md) and the draft is retired — the fold must land before Phase 2's engine swap deletes the API that document describes.
>
> **Implemented so far:** the metadata-partition commit (Commit 1 below) is on `refactor/metadata`; the streaming input seams this design's Ingress/Originate hooks ride (`stream::Segment`/`Receiver`, `dispatch_streamed`/`send_streamed`) are on the seam branches. **Settled decisions (2026-08-17, recorded in [`refactor_plan.md`](refactor_plan.md)):** the kind is named **Rewriter** and its editor **refuses** edits to BPSec-covered blocks; a Classifier **sees the deltas applied by preceding links** of the same pass; filter invocations **keep key access** (a `KeySource` argument — committed API); ipn-legacy's primary-block rewrite becomes a **config-driven fixed built-in** in the ClaSend rewrite stage; Phase 2 keeps today's per-hook drop behaviour, with formal Originate/Egress/Deliver verdict semantics deferred to Phase 3; `MetadataDelta` ships **empty of named fields** (annotation slots only) — `class`/`route_key` arrive additively with the policy and routing tranches.

## The governing constraint: a closed-source server on an unmodified open `bpa`

**In short: the pluggable filter system exists so a custom `bpa-server` can update the BPA's processing *rules* in a licence-clean manner** — against an unmodified, Apache-2.0 `bpa` consumed as an ordinary Cargo dependency, never a fork. A fork is ruled out not by the licence (Apache-2.0 would permit one) but by what it commits to: re-merging every upstream fix and release, forever. The registration seam is what makes "unmodified" sustainable, and the rules/processing split is exact: filters are the rules an embedder supplies; the pipeline is the processing the BPA owns.

It must be possible to build an alternate `bpa-server` binary in a closed-source repository that depends on the open-source `bpa` crate **completely unmodified** — an ordinary Cargo dependency, never a fork — and have that closed binary inject its own proprietary filters. A `#[cfg(feature)]`-gated filter cannot serve a closed extension (adding the cfg call-sites means editing `bpa`, i.e. forking it), so anything proprietary reaches an unmodified `bpa` through exactly one door: a **public registration API** — `bpa` exposes the filter traits, the closed `bpa-server` implements them and registers its filters at construction. The extension is **link-time, not run-time**: the Trait + register pattern Hardy already uses for CLAs, Services, and Routing Agents.

## Pipeline, not filters: the two-tier split

The previous model conflated two different things under the word "filter":

1. **The BPA's own processing** — spec-mandated and core checks. That some of these can run in parallel is an *execution detail*, not an extensibility feature.
2. **Extension points** — places where the embedding application hangs policy the open crate cannot know about. Only this needs the trait + registration surface.

The built-in checks are therefore **not registered filters — they are the pipeline**. The registration API exists for exactly one reason (closed-source embedding) and nothing internal passes through it. This is the correct reading of the netfilter analogy: conntrack and defrag hook the very same kernel points as iptables rules, but they are not rules — they are kernel processing at fixed priorities. The tables and chains are purely the *user's* surface, empty by default.

The per-layer configurability answer falls out:

- **Built-ins**: runtime *configuration* (on/off toggles, parameters in the BPA `Config`). Never registration, never reorderable — spec ordering is unviolable through the public API by construction.
- **Embedder chains**: construction-time registration, then **frozen**. Link-time embedding needs nothing more, and the frozen chain keeps the hot path lock-free.
- **Remote or dynamic registration: excluded by design, permanently.** Filters are in-process, link-time extensions only — a gRPC filter puts an RPC round-trip on the per-bundle hot path, and dynamic (un)registration reintroduces the runtime-mutable registry this design deletes. The remotable, runtime-registrable tier is the *component* tier; anything that needs those properties is a component, not a filter. Consequence: **the filter chain is locked between restarts**, making a restart the policy application boundary — see [Restart re-admission](#restart-re-admission).

Litmus test for anything ambiguous: *could an embedder legitimately want this absent or reordered?* No → pipeline code. Yes → filter.

This sits inside a wider two-tier extension model:

- **Runtime registry + sink** — for *components*: CLA, Service, RoutingAgent, AdminRecord (see `docs/acme_design.md`), keys. Register/unregister while running, gRPC-remotable, authorisation at the gRPC layer.
- **Construction-frozen registration** — for *policy supplied as code*: the filter chains, and the FlowController factories (scheduler types, bound per CLA at the peer seats — the policy/queue tranches' seam). In-process, link-time, never remotable: policy code sits on the per-bundle hot path.

Anything that appears to want runtime registration is evidence it is a component wearing a filter costume.

## Hook points: the pipeline enumerated

The evidence base for the taxonomy. Every processing point in the in→out pipeline, classified by (a) read or write, (b) externally pluggable. Stage names follow the processing blocks of [`queue_architecture.md`](queue_architecture.md); function references are current `dispatcher/` code. The ★ hook rows show the *designed* positions (today's engine runs post-store at ingress — see [Migration of existing code](#migration-of-existing-code)).

**In from a peer** (Ingest block — `receive_bundle` → `process_received_bundle` → `ingress_bundle`):

| Processing point | R/W | Pluggable? |
|---|---|---|
| CLA deframing → segment stream (`stream::Segment` via `Sink::dispatch_streamed` — landed) | write (assemble) | already — CLA trait |
| Structural header parse, canonical enforcement (`parse_headers`) | read (reject) | no — spec |
| Keyed header verify: BIB verify, BCB decrypt for extraction, NoKey liveness | read (reject) | keys already via KeyProvider/KeySource |
| Extension fields → metadata wire cache | write (meta) | no |
| Pre-drain gate: lifetime / hop exhaustion (`gate_reason`) | read (reject) | no — spec |
| **★ Ingress hook** — registered Verifiers ∥, then Classifiers | read + annotate (delta) | **yes — the hook** (headers + metadata, no payload, nothing stored) |
| Payload drain/spool (future `save_stream` seam) | write (accumulate) | no |
| Finalize: deferred block-1 BIB, §5.1.1 failure-drop, removal rewrites | **write (bytes)** | no — parser/BPSec owns |
| Persist; reception report (§5.6, before dedup); dedup | write | storage trait; reports fixed |
| Enqueue to Dispatch | queue op | no |

**In from a local application** (Originate block — `local_dispatch`/`local_dispatch_raw`):

| Processing point | R/W | Pluggable? |
|---|---|---|
| Build via `Builder`, or parse + validate raw bytes (same parser — security boundary) | write (create) / read (reject) | already — Service trait |
| **★ Originate hook** — registered Verifiers ∥, then Classifiers | read + annotate (delta) | **yes — the hook** (pre-store, in-memory) |
| Store + dedup | write | no |
| Enqueue to Dispatch | queue op | no |

**The middle** (Dispatch block):

| Processing point | R/W | Pluggable? |
|---|---|---|
| RIB lookup → Drop / AdminEndpoint / Deliver / Forward(peer) / Wait | write (queue assignment) | already — RoutingAgent |
| Admin records → Admin block (no Deliver hook — see below) | — | already — AdminRecord registry |
| Fragments → Reassemble block → re-enter Ingest processing | write | no |
| Peer-seat FlowController (egress scheduling): rate-limit + reorder by classification | read (schedule) | no — a fixed point (tc/qdisc in the netfilter analogy, not iptables) |
| Waiting / WaitingForService parking (gated queues) | queue op | no |

**Out to a peer** (ClaSend block — `forward_bundle`):

| Processing point | R/W | Pluggable? |
|---|---|---|
| Load from store | — | no |
| Per-hop rewrite: PreviousNode insert, HopCount increment, BundleAge update | **write (extension blocks)** | no — spec §4.4.x; the fixed head of the rewrite stage |
| **★ Egress hook (rewrite)** — registered Rewriters, seq | **write (extension blocks)** | **yes — the hook** (in-memory, per transmission attempt; payload/primary/BIB/BCB excluded by the editor handle) |
| **★ Egress hook (gate)** — registered Verifiers ∥ | read (reject) | **yes — the hook** (gates the final pre-BPSec wire form; re-runs with fresh context on re-route) |
| BPSec-egress seam | **write (bytes)** | no — fixed, security-policy/KeyProvider-driven |
| CLA: fragmentation + framing | write | already — CLA trait |

**Out to a local application** (Deliver block):

| Processing point | R/W | Pluggable? |
|---|---|---|
| Load from store | — | no |
| **★ Deliver hook** — registered Rewriters (seq, transport-block strip), then Verifiers ∥ | read (reject) + **write (extension blocks)** | **yes — the hook** (raw-`Service` path only; before payload decrypt) |
| BCB payload decrypt — produces the ADU; chunk-capable per KeyProvider | write (plaintext) | no — normal BPA functionality |
| `service.on_receive` | — | already — Service trait |
| Delivery report; delete | write | no — spec |

Two findings carry the whole design:

- **Payload and primary-block writes are exclusively fixed, spec-owned machinery** — the parser at ingress, BPSec at egress, decrypt at deliver, framing in the CLA; their ordering is RFC-mandated and none is a plausible external plug point. The one pluggable mutation point is the extension-block rewrite at ClaSend — where the pipeline already edits blocks per hop — and it is pluggable *by scope* (extension blocks only, via the Rewriter's editor handle), not by exception.
- **Every input-side hook is read + annotate.** Inputs never mutate wire bytes; the mutating hook capability exists only on the output side, where each hop's wire form is derived.

## The filter kinds

Three kinds. All see `&Bundle` and a `KeySource` (resolved through the existing KeyProvider seam — a Classifier reading a BCB-covered block it holds no key for takes its no-match path) — the primary block, the structural block index (header-block data and any declared payload-peek prefix resident at every invocation; **deeper payload residence never guaranteed** — the kinds are payload-independent by contract; the full residence contract is in [the sketch](#block-data--no-view-type-the-source-bytes-and-the-existing-accessors)), and the BPA metadata (provenance, wire cache, classification — see [What lives in bundle metadata](#what-lives-in-bundle-metadata)); at Egress the invocation additionally carries the resolved next hop as an argument: it is the output of the dispatch decision being executed, passed from Dispatch to ClaSend with the queued work item, not a metadata field.

| Kind | May do | Mutates | Execution | Hooks |
|---|---|---|---|---|
| **Verifier** | `Continue` / `Drop(reason)` | nothing | parallel | all |
| **Classifier** | `Continue(MetadataDelta)` / `Drop(reason)` | metadata, via delta | sequential | inputs only |
| **Rewriter** | edit extension blocks / `Drop(reason)` | extension blocks, via a scoped editor handle | sequential | **Egress + Deliver** |

**The mutation boundary is the block, not "bytes".** Extension blocks are BPv7's own extensibility surface — where the protocol itself says new wire-visible behaviour lives — so a filter system whose purpose is licence-clean custom processing rules must be able to add, modify, and remove them. What filters can never touch: the **payload block** (the application's data, owned end-to-end by the application, BPSec, and the CLA), the **primary block** (immutable per RFC 9171), and **security blocks** (BIB/BCB — the BPSec seams' monopoly). The Rewriter's editor handle exposes exactly the permitted operations, making payload-purity a compile-time property rather than a code-review promise, and refuses edits to blocks under existing BPSec coverage (this node is not their security source).

The two mutating kinds are duals by scope: the **Classifier** (inputs) writes *node-scoped* annotations — metadata this node's own downstream consumes; the **Rewriter** (egress) writes *network-scoped* annotations — extension blocks the next hops consume. A rule whose effect is local needs a delta; a rule whose effect must travel needs a block.

All three kinds are payload-free and therefore **streaming-immune**: at ingress, Verifiers and Classifiers run on the accumulation buffer before the payload is drained, and when the full streaming gate lands (`streaming_pipeline_design.md` §5.4/§5.7) they ride it unchanged; the Rewriter edits header blocks, which are resident at ClaSend where the per-hop rewrite already operates, while the payload streams past untouched. There is no late ingress pass — an early/late Ingress split would exist only to serve in-pipeline payload inspection, and with none there is nothing a late pass could see that the pre-drain pass cannot. The staged structure lives where it belongs: the pipeline's own pre-drain gate / post-drain finalize seam, fixed processing rather than API.

The Classifier returns a *delta* rather than taking `&mut metadata` deliberately: the engine applies it, filters never touch `bundle.metadata` directly, the boundary stays clean for closed-source implementors, and delta application is idempotent — which the queue architecture's at-least-once semantics require of every processing block.

The Rewriter's execution model is **in-memory, per transmission attempt** — exactly the semantics of the built-in per-hop rewrite it extends. The stored bundle remains as received (post-parser canonicalisation); each hop's wire form is derived fresh at ClaSend and never written back. That single choice is what keeps the rest of the design intact: no persisted filter mutations means no crash window between rewrite and enqueue, restart re-admission stays metadata-only, and a re-routed bundle is re-prepared for its new peer with fresh context.

## Hooks and the matrix

| Hook | Processing block | Position | Verifier | Classifier | Rewriter |
|---|---|---|:--:|:--:|:--:|
| **Ingress** | Ingest | pre-drain, pre-store (at the gate) | ✓ | ✓ | — |
| **Originate** | Originate | pre-store, in-memory | ✓ | ✓ | — |
| **Egress** | ClaSend | after per-hop rewrite, before BPSec | ✓ | — | ✓ |
| **Deliver** | Deliver | before payload decrypt | ✓ | — | — |

**Classifier is inputs-only.** Classification annotates metadata that the node's *own downstream* consumes — the traffic class whose properties drive dispatch weighting, egress contracts, eviction, and table selection, plus the routing key and any registered annotation slots. There is no downstream inside the BPA past an output boundary, so an Egress or Deliver Classifier has nothing to feed.

**Rewriter is egress + deliver — the two output boundaries.** Inputs never rewrite — incoming wire bytes are canonical truth, rejected at parse, never rewritten (`streaming_pipeline_design.md` §5.2.2) — and a locally-originated bundle's blocks are the Builder/service tier's to assemble. Wire preparation for the next hop belongs at the one point with next-hop context, per transmission attempt; that also covers locally-originated traffic, since Originate→Egress passes through ClaSend like everything else. Deliver is the *dual*: it strips **transport-scoped** extension blocks — network QoS, custody, the per-hop plumbing ("transport headers", to borrow HTTP's hop-by-hop/end-to-end split) — so a terminating bundle hands *content* to the application, not network bookkeeping (a security property too: Previous Node and internal QoS policy do not leak to an untrusted app). This applies only to the raw-bundle `Service` path; the payload-only `Application` path already sees no extension blocks. Because the Rewriter edits extension blocks and never the payload, the Deliver invocation sits *before* the payload's BPSec decrypt and uses its `KeySource` to decrypt any extension block it must inspect. Next-hop context is Egress-only (a delivering bundle has no next hop), so it rides the `RewriteContext::Egress` variant rather than the method signature. The standard RFC-defined transport blocks (Previous Node, Hop Count, Bundle Age) may instead be stripped by fixed machinery; the pluggable Deliver Rewriter is for embedder-defined transport blocks — the dual of the ingress Classifier that consumed them (e.g. a custom QoS block read at ingress, stripped at deliver).

**There is no FORWARD hook.** The pipeline has exactly one out-to-peer processing point — locally-originated and transit traffic converge before ClaSend — so the output hook is netfilter's POSTROUTING, not its FORWARD. Netfilter needs a separate FORWARD chain because flat rule tables cannot ask "is this transit?"; our filters are code with metadata access, and the persisted provenance (`origin` is `Ingress` — see [What lives in bundle metadata](#what-lives-in-bundle-metadata)) at Egress *is* the transit predicate. The two things a FORWARD hook would add beyond that predicate have better homes: acting once per routing decision is an Ingress matter (provenance never changes after ingest), and changing where a bundle goes is the RoutingAgent's job — a filter that redirects is a routing agent in a costume. The hook keeps the name **Egress** deliberately: it does *not* have netfilter-FORWARD semantics (it sees originated traffic too), and naming it Forward would import the transit-only intuition.

**The admin path is hook-free by design.** Admin records addressed to this node terminate in the Admin block, whose extension surface is the **AdminRecord registry** (registry + sink keyed by record type, with record-type-claim authorisation at the gRPC layer — `docs/acme_design.md` §4). Filters never intercept the control plane's terminal processing; admin bundles still cross Ingress like any other bundle, so boundary policing applies.

## Where bytes change

**Payload and primary-block mutation is exclusively fixed machinery**; extension-block mutation has exactly one pluggable point, inside the egress sequence:

```
load from store                          (stored bundle = as received; source of truth)
  └▶ built-in per-hop rewrite            PreviousNode / HopCount / BundleAge — fixed, spec §4.4.x
                                         + the config-driven legacy-EID re-encode (see Migration)
  └▶ registered Rewriters (seq)          extension-block add / modify / remove — THE pluggable mutation point
  └▶ Egress Verifiers (∥)                gate the final pre-BPSec wire form
  └▶ BPSec-egress seam                   add BIB/BCB per security policy — fixed, KeyProvider-driven
  └▶ CLA                                 fragmentation + framing — fixed link adaptation
```

The Rewriters sit where they do for two reasons: after the built-in per-hop rewrite so they act on the near-final form (and are, semantically, an extension of that stage — the built-ins are simply the fixed head of it), and before the BPSec seam so the blocks they add or modify can be signed per security policy. Verifiers run after the Rewriters so they gate what actually ships.

The fixed byte-owners elsewhere are unchanged:

- **Ingress**: the parser — canonical rejection (never rewrite incoming wire bytes), BPSec verify/decrypt, the RFC 9172 §5.1.1 removal cascade.
- **Deliver**: BCB payload decrypt (chunk-wise where the KeyProvider and cipher permit) — normal BPA functionality.
- **Re-targeting** (BIBE, tunnelling, overlays): RIB-selected **virtual CLAs**. The carrier gets a new destination, so the next lookup makes forward progress; chaining is the sequence of carrier destinations; loop protection is ordinary hop-count/age.

There is no payload-rewriting egress Transform. The slot such a filter would reserve belongs to BPSec (a fixed seam), and its other tenants are payload operations with better homes: compression and framing are the CLA's, transcoding/redaction are rewriting gateways (components, next section), aggregation is application-layer or BIBE-shaped encapsulation. The Rewriter is not a Transform — it is scoped to extension blocks, extends a stage that already mutates them, and its output remains subject to the fixed BPSec seam behind it. The `WriteResult` byte-return path and the re-validation re-parse it forces go with it (see [Migration](#migration-of-existing-code)).

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

Two component shapes in Hardy terms:

1. **Queue/verdict (the NFQUEUE analogue — preferred).** Under the queue architecture, forward-to-scanner is an *enqueue to a scanner-owned queue*; the scanner is a registry+sink component that consumes bundle bytes and returns a verdict — release with an optional classification delta, or drop with a reason — which re-enqueues to Dispatch. No re-ingress, no dedup collision, parking is a queue doing what queues do, and the component is runtime-registrable and remotable without touching the frozen filter chains, because traffic only reaches it by explicit RIB/policy selection.
2. **Provenance-chained peer (the BIBE-adjacent shape).** RIB policy forwards selected traffic to a scanner peer; re-injected traffic is distinguished by provenance (its `origin` records arrival from the scanner) so the second lookup does not re-select it. Caveat: a successful CLA forward is terminal today (report, delete, tombstone), so a *same-bundle* round-trip collides with dedup on re-entry — BIBE dodges this only because the carrier is a new bundle. This shape needs the virtual-CLA **re-forward entry point** (non-terminal forward semantics) from the routing work.

**One bounded case stays inside the filter family: payload header peeking.** A Classifier may need the first few bytes of the payload — a wrapped IP header, an HTTP request line — purely to place a class: netfilter's `-m u32` shallow match, not its NFQUEUE. This is *bounded classification input*, not payload processing, and it fails none of the three tests above: there is no engine, no parking, and no verdict latency (a match on already-resident bytes); a ≤P-byte protocol-header decode is the same in-process risk class as the BPA's own CBOR parsing; and where BPSec encrypts the payload the peek reads ciphertext and classifies nothing — a deployment property the registering embedder knows. Mechanically it is free, because at every wire-facing hook the initial bytes of the bundle are memory-resident anyway — ingress is accumulating them, egress/deliver has loaded them to emit them — so the ingress drain simply keeps the first min(P, payload length) payload bytes on the invocation side of the spool boundary instead of discarding them, where P is fixed at `build()` as the maximum prefix declared across the Builder registration calls (an argument of the `add_*` invocation, like everything else about a filter's wiring; default 0, so the zero-config node retains nothing). Nothing is cached or persisted: the peek exists to stamp a class at classification time, and its bytes are no more carried alongside the bundle than the header-block bytes are. The DPI line stays bright and *is* the bound: input bounded at `build()` → Classifier; unbounded or verdict-driven processing → component. Two inherent caveats: only an offset-0 fragment carries a meaningful prefix (others classify without it; the reassembled bundle re-crosses Ingest and is peeked properly), and a BCB-encrypted payload is the classifier's no-match path.

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

## Alignment with the queue architecture

- Hooks land 1:1 in processing blocks (matrix above). Egress filters run in ClaSend, downstream of peer-seat FlowController scheduling.
- Status transitions become queue assignment (`enqueue` is the atomic commit point), so the metadata that remains is provenance and the wire cache plus filter-set classification: **filters annotate classification; the BPA owns lifecycle via queues.**
- Classification assigns the bundle's traffic class (see [`MetadataDelta` and the traffic class](#metadatadelta-and-the-traffic-class)); the FlowController's `push` places the bundle in its class queue, and each FlowController reads the class's properties from the frozen `ClassPolicy` — no translation anywhere. (This supersedes `queue_architecture.md`'s single-`flow_label` model — the queue doc's own principle survives in sharpened form: filters assign, the pipeline consumes.)
- At-least-once semantics make every hook chain re-runnable after a crash. Byte-pure verdicts and idempotent delta application satisfy this for free — and the queue doc's known crash window (byte rewrite vs enqueue) ceases to exist, because no filter mutation is ever persisted — input hooks write only deltas, and Rewriter edits are per-attempt and in-memory.

## Restart re-admission

Because the chain is frozen in-process, filter policy can only change across a restart — a new binary, new construction wiring, or new `Config`. Stored bundles were admitted and classified by the *previous* chain, so **restart recovery re-runs the input-hook chains over stored bundles**: new policy applies to traffic already in custody, not just to new arrivals.

What re-runs, precisely:

- **Input hooks only.** Egress and Deliver Verifiers execute per transmission/delivery attempt, so they apply current policy naturally — and egress Rewriters are outside re-admission entirely: their edits are per-attempt and never persisted, so there is nothing stale to re-run and the next attempt uses the current chain by construction. It is the input hooks whose effects persist — the admission verdict and the classification — and provenance picks the chain: ingress-entered bundles re-cross the Ingress chain, originated bundles the Originate chain.
- **Classification is re-derived from scratch.** Every persisted delta field is a cache of the chain's output, not an input to it; re-admission clears and re-derives them all, so removing a Classifier removes its annotations.
- **A Verifier drop at re-admission is a deletion in custody** — deletion report per the bundle's flags, never a fresh reception report (reception was reported once, at arrival). For originated bundles this is also the only notification path — the originator's `send` succeeded long ago.
- **The config-gated built-ins join the same pass.** `Config` is as restart-locked as the chain, so a tightened `primary-block-integrity` applies to stored bundles by the same rule — and those checks read the structural index, so they are equally metadata-only.
- **Fragments** re-cross the full Ingest processing via the reassembly path, unchanged.

The payload-free kinds are what keep this affordable: re-admission never loads a payload. The recovered record carries the structural block index and the metadata groups but no wire bytes; a Classifier that reads block bodies (a UQEB reader, a payload peek) still gets them, because the engine supplies the invocation `data` by a **bounded head read** from `BundleStorage` — the persisted extents say how much is needed (bundle start through payload data start + P), and no new storage primitive is required: the engine calls the ordinary sequential `load` and drops its receiver once it has those bytes, which the backend observes and stops. One bounded read per stale bundle, paid lazily at the Dispatch crossing. Re-admission is intentionally slow path — recovery after a crash, or a policy-epoch bump — and one bounded read per bundle is the right trade against degraded classification: filters re-run with the same invocation context as at ingress and cannot tell the difference. A node with no registered input filters reads nothing and the pass stays metadata-only (the config-gated built-ins read the structural index). A filter's behaviour can change without its construction wiring changing shape (same registration code, new binary), so change detection is impossible — every restart conservatively re-admits everything. The seat is **lazy: a per-bundle stamp checked at the Dispatch block**, which every bundle already flows through (Waiting/WFS sweeps, originate, re-ingest). The stamp is a **policy epoch**, not a boot id: a restart bumps it, and so does a runtime class-policy push from a centralized policy manager (policy *data* flows at runtime through the component tier; only policy *code* rides the restart boundary) — the same mechanism re-derives classification in both cases, which an eager restart-time walk could never do for pushes. Accepted cost: a tightened Verifier purges a Waiting bundle only when a sweep next moves it; a background walk can close that gap if storage-pressure purging matters.

## `MetadataDelta` and the traffic class

The delta carries two fields, and the distinction between them is the design:

| Field | What it is |
|---|---|
| `class: Option<ClassId>` | membership of a **traffic class** — the unit of differentiated treatment |
| `route_key: Option<Eid>` | per-bundle routing key ([`routing_table_redesign.md`](routing_table_redesign.md)) |

**The class is the unit of policy.** A traffic class is defined once — in the `[classes]` configuration, or by an embedder in Builder code — together with *all* of its per-dimension properties: dispatch weight, egress contracts (floor/ceiling percentages of contact volume), eviction rank, routing table. `build()` compiles the definitions into one frozen `ClassPolicy` table, and every consumer — the dispatch-seat FlowController, each peer-seat FlowController instance, eviction, table selection — reads the bundle's class properties by field access on that shared, validated object. There is no per-consumption-point configuration, and nothing is looked up at runtime beyond an array index into frozen policy. The class definition, its properties, and their consumers are the policy redesign's subject ([`policy_subsystem_redesign.md`](policy_subsystem_redesign.md)); this document owns only the delta seam.

Two simpler shapes fail in opposite directions, and the class model is the fixed point between them:

- **A single opaque label** (`flow_label`, per `queue_architecture.md`) put an uninterpreted tag in metadata and a *separately configured* translation map at every consumption point — N config surfaces, N unmapped-value failure modes, and per-point config languages creeping toward a second rule system.
- **Per-dimension fields** (`priority`, `traffic_class`, `eviction_priority`, `route_table`) eliminated the maps but made the classifier explode one decision into many fields — and the fields kept collapsing into each other under scrutiny because they were all the same class identity, differently named per consumer. A distinct treatment *is* a class; per-bundle values for enumerable dimensions add no expressive power.

One semantic identity in metadata, one definition owning all its properties, zero translation surfaces. This is also the correct reading of the tc precedent: iptables `-j CLASSIFY` sets `skb->priority` — a *class handle* whose meaning (rates, ceilings) lives in the qdisc's class definitions, in one place — whereas the rejected shape is `-j MARK` plus per-device filter rules re-interpreting the mark at every qdisc.

**`route_key` remains a direct field because it fails the enumerability test** that justifies the class: an `Eid` (a label-stack top, a virtual class EID) is per-bundle data no finite class set can carry. That was the original tell against the opaque label, and it survives as the criterion for all future delta growth: **enumerable treatment → class property; unbounded per-bundle data → delta field.**

The admission tests recorded previously migrate from delta fields to **class properties**, where duplication is impossible by construction: `egress.floor`/`ceil` land on `ClassPolicy` with the policy tranche if a deployment needs allocation floors orthogonal to dispatch weight (the starvation-floor test); an eviction rank lands with the storage-pressure tranche if retention value diverges from urgency (the inversion test: real-time telemetry, high urgency, worthless once stale — evict first); the routing `table` property lands with the routing tranche. Both `ClassPolicy` and `MetadataDelta` are `#[non_exhaustive]`, so growth is additive, never semver-breaking. Dispatch consumes the class weight by weighted-fair dequeue — no strict tiers, no starvation; the scheduling disciplines themselves are queue/policy-tranche material.

Mechanics (phasing settled 2026-08-17: the delta ships in Phase 2 with annotation slots only — `class` and `route_key` arrive additively with the policy and routing tranches, which `#[non_exhaustive]` makes non-breaking): per-field last-writer-wins across the sequential Classifier chain; both fields are serde-persisted in the classification group and re-derived at restart re-admission — each persisted value is a cache of the chain's output, which also self-heals class-definition changes across a restart. The `ClassId` representation (stable name vs table index, and its interaction with the policy-epoch stamp) is a Phase-2 detail. With no classes configured the field is never set, every consumer sees the default class, and the node behaves exactly as today — the zero-config baseline.

### Annotation slots — embedder-private metadata

Custom filter pairs — an ingress Classifier and an egress Rewriter shipped together — need to carry vendor-private intermediate state from admission to transmission: parse a proprietary extension block once at ingress, act on it at egress. The delta therefore supports **registered annotation slots**: at construction, alongside registering its filters, an embedder registers a slot (stable name + value type + serde) and receives a typed `SlotHandle<T>`; `build()` rejects name collisions loudly, and a per-slot size bound at delta application keeps metadata stores honest. Classifiers write a slot through the delta (per-slot last-writer-wins, like every delta field); any filter holding the handle reads it. **The handle is the capability**: possession grants access, so a pair shares state by sharing the handle in its common construction code — privacy by unforgeability, no permission machinery. The BPA carries the values opaquely ("duck-typed" from its perspective); the pair sees them fully typed. (Precedents: `http::Extensions` is Rust's middleware-pair pattern, but `TypeId` keys cannot persist; DPDK's registered mbuf dynfields supply the registration-at-init model; persistence forces the combination — stable registered names outside, typed handles inside.)

Slots inherit the classification group's semantics wholesale, which imposes the one contract: **a slot value is a cache of a pure derivation over (stored bytes, chain, config) — never a ledger.** Persisted with the bundle (an egress Rewriter may read it days after admission); cleared and re-derived at restart re-admission and policy-epoch bumps; a load meeting an unregistered name (the embedder changed across a restart) drops it harmlessly — re-admission re-derives. Accumulating state, or anything that must travel between nodes, is not slot material: it belongs on the wire as an extension block, written by the Rewriter per transmission attempt. The node-scoped/network-scoped dual is unchanged — slots are the pair's node-scoped scratch; the Rewriter materialises the network-scoped result.

The delta-growth criterion gains its third clause: enumerable treatment → class property; BPA-defined unbounded per-bundle data → named delta field; **embedder-defined data → annotation slot**. This also settles the old marks / "trace mark" question: a would-be mark consumer registers a slot, and inter-filter signalling is the mechanism working as designed — no shared mark set exists, and `flow_label` stays retired.

Slots ship whole with Phase 2 — nothing lands early (settled 2026-08-20). The storage side is separable from the filter traits in principle, but the parts with design content — the frozen slot table from `build()`, stable-name keying at rest vs index-keyed handles at runtime (the name↔index translation at metadata load), per-slot LWW and the size bound at delta application, epoch-driven clearing — are engine and Builder semantics that can only be reviewed against a real Classifier, and the metadata-partition record accommodates them additively (`Classification` is private and serde-additive), so early landing buys nothing. No early consumer exists: the one conceivable pre-Phase-2 ingress→egress pair is BPSec machinery, and that is *pipeline*, not a registered filter — a built-in carries its node-scoped state (deferred verification results, NoKey watch state) in crate-private metadata fields directly, ordinary Rust privacy giving built-ins what the unforgeable handle gives embedders. That a built-in pair has no use for slots is the two-tier litmus test passing, not a gap.

## What lives in bundle metadata

The delta decision forces the wider question, and the answer is a principle: **metadata holds write-once facts, caches of pure derivations, and BPA infrastructure references — never independent mutable state.** Everything in it is either a historical fact or recomputable from (stored bytes, frozen chain, config), so nothing can be torn by a crash and the at-least-once story stays trivial.

| Group | Fields | Written by | Persisted | On restart | Filter visibility |
|---|---|---|---|---|---|
| **Provenance** | `received_at`; `origin: Ingress { peer_node, peer_addr, cla } \| Originated \| Recovered` | admission machinery, once | yes | kept — historical fact | read-only |
| **Wire cache** | `previous_node`, `age`, `hop_count` | parser, from the stored bytes | yes | kept — stored bytes never change | read-only |
| **Classification** | `class`, `route_key`, registered annotation slots, plus the policy-epoch stamp (lazy re-admission) | Classifier chain, via applied deltas | yes | cleared + re-derived (re-admission) | read; written only via the delta; slots gated per-handle; the epoch stamp invisible (engine bookkeeping) |
| **Infrastructure** | `storage_name` | BPA | yes | kept | **none — absent from the view** |

**Visibility is a property of each group**, as load-bearing as its writer, persistence, and restart fate — and it is enforced the way the Rewriter's payload-purity is: by construction, not code review. Filters receive a *projection* of the metadata — a view type from which invisible fields are simply absent — never a reference to the record plus a rule about which parts to ignore. Infrastructure stays BPA-private for safety, in three distinct senses — where "safety" means accident-prevention among trusted code, not defence: filters are in-process, link-time, embedder-supplied (the registration seam is a licence boundary, not a privilege boundary), so visibility rules are ordinary Rust privacy discipline doing what it always does. First, a mechanism reference in filter hands invites out-of-contract coupling: code keyed on `storage_name` has bound itself to storage internals it cannot see change, and will break — or worse, quietly misbehave — when they do. Second, whatever the view exposes to a closed embedder is committed semver surface — exposing internal representation freezes it forever, while an invisible field remains the BPA's to rename or restructure at will. Third, policy written against mechanism internals is meaningless policy: a rule keyed on a storage backend's naming scheme is not a rule about the bundle, and the view's shape should make such rules inexpressible rather than merely inadvisable. The same axis recurs at finer grain inside the classification group: the policy-epoch stamp is engine bookkeeping no filter can read, and each annotation slot is visible only to holders of its `SlotHandle` — per-slot visibility by unforgeability, no ACL machinery.

Two fields leave the metadata record entirely:

- **`status`** is queue assignment (`queue_architecture.md`); its `serde(skip)` today shows the move half-made — the queue redesign completes it.
- **`next_hop`** is a per-dispatch transient: computed by the RIB lookup, consumed by ClaSend, carried in the Dispatch→ClaSend queue entry rather than in metadata. It is never persisted (as today), and any re-dispatch recomputes it — consistent with re-routing semantics.

Making provenance explicit fixes a latent bug: `ingress_cla` is currently transient (`serde(skip)`), so after a restart both the Egress transit predicate and re-admission's chain selection would mis-read a recovered bundle as locally originated. Provenance is **persisted, write-once**: the `origin` enum records the arrival facts durably (the CLA name is a fact about arrival even if that CLA instance no longer exists), makes the transit predicate a type-level match, and gives restart re-admission its chain selector.

`WritableMetadata` retires with `flow_label`: the classification group replaces it, and it is written only by the engine applying deltas — filters never touch metadata directly. Expiry remains a non-field — derived from the creation timestamp, lifetime, age, and (for unclocked sources) `received_at`, indexed at the storage layer for the reaper. The visibility column is also the substance of the exposed-view answer: filters read provenance and the wire cache, read and delta-write classification (slots per handle), and infrastructure does not exist in their world — what remains open is only the concrete Rust shape of that projection.

## Sketch — the concrete types

A working sketch of open question 1, to make the visibility and partition rules concrete enough to review — and to anchor the metadata-partition commit. The right starting point is not the metadata record but **the type that gets put in the queue**, because two former metadata fields dissolve into it.

### The queue item — custody is ownership

```rust
/// What Dispatch's FlowController holds and drains into the processing pool.
pub struct DispatchItem {
    pub bundle: Bundle,
}

/// What `Forward(peer)` pushes onto the peer seat, and ClaSend pops.
pub struct ForwardItem {
    pub bundle: Bundle,
    /// The resolved dispatch decision — travels with the work, never in metadata.
    pub next_hop: NextHop,
}

/// The resolved adjacency (today's rib Forward payload, made a type).
pub struct NextHop {
    pub peer: Eid,
    pub cla_addr: ClaAddress,
}
```

The item owns `Bundle` **by value** — not `Arc`, not a storage id. Exactly one queue or processing block holds a bundle at any moment, and moving `Bundle` through queue items makes that single-custody invariant a move-semantics fact: an aliased second custodian is unrepresentable, the same compile-time trick as the Rewriter's scoped handle. It also costs no metadata-store read per hop — the durable copy is the metadata store plus the queue-assignment record, and restart recovery reconstructs in-flight items from those. `Arc<Bundle>` would buy cheap clones and cost exactly the invariant.

This is where `status` and `next_hop` die as fields: *which item type the bundle is inside of* *is* the status, and `next_hop` is a field of `ForwardItem` — both former metadata passengers become facts about position in the pipeline. Egress filters receive the item's `next_hop` as invocation context, closing the loop with [The filter kinds](#the-filter-kinds).

### The metadata record — visibility by privacy, not by view struct

```rust
pub struct Bundle {
    pub bundle: hardy_bpv7::Bundle,   // structural: primary + block index
    pub metadata: BundleMetadata,
}

pub struct BundleMetadata {
    // write-once provenance: private fields + pub read accessors
    received_at: OffsetDateTime,
    origin: Origin,
    pub extensions: ExtensionFields,  // plain pub — read-only comes from &
    classification: Classification,   // private: getters to read, apply() to write
    pub(crate) storage_name: Option<Arc<str>>,   // infrastructure — unreachable outside the crate
}
```

*(As landed by the partition commit: the provenance fields sit inline rather than in a `Provenance` struct, and `storage_name` stays a bare `pub(crate)` field — a one-field `Infrastructure` struct added ceremony without enforcement. `Classification` landed as the empty private placeholder; slots and the epoch arrive with Phase 2.)*

The visibility column maps onto Rust field/module privacy — no separate per-invocation view struct at all. Filters (closed-source, external crate) receive `&Bundle`: `extensions` is directly readable, provenance and classification are readable through getters, and `infra` is unreachable outside the crate. The record's own privacy *is* the projection, so there is no view type to drift from the record. The consequence — bpa-internal code also goes through the accessors — is the ~35-site mechanical sweep the metadata-partition commit already budgets.

**`bpa::bundle::Bundle` is committed filter API, directly.** This is per-bundle fast-path code, and the record-with-privacy shape is the only zero-cost projection: private fields plus inline getters compile to field reads. The alternatives each tax every invocation — a wrapper struct is built per filter per bundle, a trait view adds dynamic dispatch per field read — to buy insulation the privacy boundary already provides for everything not deliberately exposed. What the commitment actually freezes is only the enumerated public surface ([below](#what-the-sketch-settles-and-what-it-phases)); the private group internals remain free to restructure, which is most of the record.

```rust
pub struct Provenance {
    received_at: OffsetDateTime,
    origin: Origin,
}

pub enum Origin {
    Ingress {
        peer_node: Option<NodeId>,
        peer_addr: Option<ClaAddress>,
        cla: Arc<str>,               // now persisted — the latent-bug fix
    },
    Originated,
    /// Recovered from bundle storage without a metadata record; the
    /// arrival facts are unrecoverable. Truthful where fabricating an
    /// Ingress origin would be the exact lie provenance exists to kill.
    Recovered,
}

impl BundleMetadata {
    /// The only constructors — no Default; a defaulted provenance would
    /// fabricate a received_at and an origin. (`new(received_at, origin)`
    /// is the explicit-parts primitive for record reconstruction —
    /// storage backends, fixtures, recovery paths.)
    pub fn ingress(cla: Arc<str>, peer_node: Option<NodeId>, peer_addr: Option<ClaAddress>) -> Self { /* … */ }
    pub fn originated() -> Self { /* … */ }
    pub fn new(received_at: OffsetDateTime, origin: Origin) -> Self { /* … */ }
}
```

Write-once is private fields + constructors + no `&mut` accessor, ever. The transit predicate is `matches!(m.origin(), Origin::Ingress { .. })`.

```rust
pub struct ExtensionFields {
    pub previous_node: Option<Eid>,
    pub age: Option<core::time::Duration>,
    pub hop_count: Option<HopInfo>,
}
```

Plain pub fields suffice: filters only ever hold `&Bundle`, so immutability falls out of the reference, and the parser (in-crate) writes the cache at parse time. `creation_time()` / `expiry()` / `previous_node()` on `Bundle` re-express over `provenance.received_at()` + `wire.age` — same logic, new paths.

### Classification — three privacies in one group

```rust
pub struct Classification {
    class: Option<ClassId>,
    route_key: Option<Eid>,
    slots: SlotMap,                   // slot index → serialized value
    epoch: PolicyEpoch,               // engine bookkeeping — no accessor at all
}

impl BundleMetadata {
    pub fn class(&self) -> Option<ClassId> { /* … */ }
    pub fn route_key(&self) -> Option<&Eid> { /* … */ }
    /// Handle-gated slot read — the capability model in one signature.
    pub fn slot<T: SlotValue>(&self, handle: &SlotHandle<T>) -> Option<T> { /* … */ }

    /// The engine's write path — the ONLY write path.
    pub(crate) fn apply(&mut self, delta: MetadataDelta) { /* per-field LWW, idempotent */ }
    pub(crate) fn clear_classification(&mut self, epoch: PolicyEpoch) { /* re-admission */ }
}
```

Each privacy grade uses a different Rust mechanism: `class`/`route_key` are public via getters; slots are public *only through the unforgeable handle* (`SlotHandle<T>` = slot index + `PhantomData<T>`, obtainable solely from Builder registration — possession is the ACL); `epoch` has no accessor and does not exist outside the crate. Persistence forces `SlotMap` to hold serialized bytes at rest (the registered serde does the coding); `slot()` decodes on read, which matches the cache-of-a-pure-derivation contract — a decode-per-read of a small bounded value is cheap and never authoritative.

```rust
#[non_exhaustive]
#[derive(Default)]
pub struct MetadataDelta {
    pub class: Option<ClassId>,
    pub route_key: Option<Eid>,
    slots: SlotDeltas,                // written only via set()
}

impl MetadataDelta {
    pub fn set<T: SlotValue>(&mut self, handle: &SlotHandle<T>, value: T) { /* … */ }
}
```

### Block data — no view type; the source bytes and the existing accessors

`&Bundle` gives a filter the block *index* (`hardy_bpv7::Bundle.blocks` — types, flags, numbers, extents), and bpv7's accessors already take the source bytes as an argument: `Block::payload(source) -> Option<&[u8]>` (bundle-absolute offsets, bounds-checked) and `Block::extract::<T>(source)` (CBOR decode with smuggling check), both of which return `None`/`Ok(None)` when the block's bytes are not resident in `source` — the headers-only buffer case is already in their contract. So the invocation passes the resident source bytes and nothing new:

```rust
// Verifier:   fn check(&self, bundle: &Bundle, data: &[u8], keys: &dyn KeySource) -> Verdict
// Classifier: fn classify(&self, bundle: &Bundle, data: &[u8], keys: &dyn KeySource) -> Result<MetadataDelta, Drop>
// Egress invocations additionally receive the queue item's next_hop.
```

No view type mediates byte access, because **filters are trusted code** — in-process, link-time, supplied by the embedder; the registration seam is a *licence* boundary, not a *privilege* boundary, and there is no security boundary inside a process for a wrapper type to enforce. What is real is the **residence contract**: every invocation sees the header blocks plus the first P declared peek bytes ([payload header peeking](#payload-inspection-is-a-component-not-a-filter-kind)) — at the wire-facing hooks because the pipeline is receiving or emitting those bytes anyway, and at restart re-admission via a bounded head read from bundle storage (re-admission is intentionally slow path; the persisted extents give the exact range) — while payload residence beyond P is never guaranteed (hook position, streaming state). The accessors' optional returns express the variability uniformly, and a filter cannot tell a re-admission invocation from a live one. The payload-free property of the kinds is an architectural contract, not a wall: a filter that *depends* on payload bytes has written itself out of streaming-immunity, and the design's answer to payload-hungry processing remains the component tier ([Payload inspection is a component](#payload-inspection-is-a-component-not-a-filter-kind)) — for encryption, latency, and isolation reasons that hold regardless of what the API could physically prevent.

This also keeps the byte-access surface at zero: the UQEB Classifier calls `bundle.blocks[&n].extract::<UqebParams>(data)` — the same accessor bpa-internal code uses. `&[u8]` rather than `bytes::Bytes` because the borrow matches the residence contract (bytes valid for the invocation; keeping them means an explicit copy or a decode into owned values, which is what a real Classifier does anyway), while `Bytes` implies a shareable owned contiguous buffer — a promise the streaming end-state does not make. `Bytes` stays where the BPA genuinely transfers buffer ownership (`Sink::dispatch`, storage `save`/`load`).

### What the sketch settles, and what it phases

The committed semver surface enumerates precisely: `Bundle`'s two pub fields, `ExtensionFields`'s pub fields, the getter set (`origin`, `received_at`, `class`, `route_key`, `slot`), `MetadataDelta` + `SlotHandle::set`, the `data: &[u8]` source argument (block-body access via bpv7's existing `payload`/`extract` accessors — already committed bpv7 API), `NextHop`, and the three filter traits. Everything else — group internals, `SlotMap` representation, the epoch, infrastructure — stays the BPA's to restructure.

Phasing: the **metadata-partition commit** takes the record shape — the four groups, private fields + accessors, the constructors, `Origin` with `cla` persisted, no `Default`, `next_hop` demoted to a transient — while `status` and the flat `writable` group survive as interim passengers until their tranches. The **queue items** and `MetadataDelta`/slots are Phase 2/queue-tranche material; the sketch fixes their shape so the partition commit cuts the record along the right seams.

Open residue: whether `ForwardItem.bundle` stays a full `Bundle` or leans down once the queue tranche makes queues durable (start with `Bundle`, let the queue work argue for less), and the intra-chain classification read question in [Open questions](#open-questions). Refinement (2026-09-01): the `NextHop` associates with the **egress queue itself** rather than riding each item — a peer queue exists per dispatch decision, so once a bundle is in the queue its destination is a property of the queue; this also retires the interim `metadata.next_hop` transient and `forward_bundle`'s missing-hop re-dispatch guard (the hybrid channel's storage spill cannot carry a per-item transient, but queue-level state survives it).

## Worked example — segment routing

Segment routing exercises the whole seam, licence-clean: an input Classifier derives the effective top of a label-stack extension block (skipping segments equal to self, so the RIB never sees key == self and deliver-vs-forward stays keyed on the real destination) and sets `route_key`; an egress Rewriter pops consumed segments onto the wire — the first known Rewriter consumer. Forward progress generalises from "re-targeting" to **"the lookup key is consumed"** ([`routing_table_redesign.md`](routing_table_redesign.md), which owns the RIB side: `route_key.unwrap_or(destination)`, tables, the FIB compilation). BPv7 block flags give strict vs best-effort waypoint semantics for free, and the label block joins the not-BIB-covered / `must_replicate` convention of the mutable per-hop family.

The per-attempt Rewriter model makes the pop restart-safe by construction: it commits only by transmission — the stored copy is never mutated — so no torn stack is ever persisted, at-least-once retransmissions re-derive byte-identical wire forms, and the frozen chain plus restart re-admission guarantee the classifying half and the popping half of the rule never skew. (Fragment residual-stack reassembly is a one-line rule for the eventual SR block spec, and a transitional one: IETF/CCSDS intend to deprecate RFC 9171 ADU fragmentation once BIBE standardises as its replacement.)

## Worked example — ESA's QoS extension block (UQEB)

Where segment routing exercises the Rewriter, ESA's QoS proposal (`draft-algarra-dtn-qos`, the User QoS Extension Block — source-added, transit-immutable, BIB-protected, carrying traffic priority, retransmission preference, latest-only delivery, and retention class) exercises the Classifier seam and the class model end-to-end, and it is worth recording how each parameter lands.

The whole deployment is **one ingress/originate Classifier plus class definitions** — no new primitives. The Classifier reads the UQEB from the block index (the keyed header verify runs before the Ingress hook, so it sees a BIB-verified block — the draft's integrity MUST is satisfied by pipeline ordering) and maps `(source, requested parameters)` to a class. Because the draft's "user" is an SLA-contracted entity, the class set is the configured SLA profiles — a-priori configuration, safely enumerable despite the parameter cross-product — and clamping the requested tuple to the user's contracted class *is* the draft's Security Considerations policing ("ignore the requested handling" for unauthorized parameters): authorization and classification are the same operation. No Rewriter is involved: the UQEB is source-only and MUST NOT be modified in transit, so the one QoS block actually proposed for BPv7 never needs the mutation primitive.

Three of the four parameters are class properties consumed downstream, exactly per the criterion:

- **Traffic priority** → dispatch/peer-seat scheduling. The draft's strict per-user precedence differs from the open crate's weighted-fair default, and that is the policy split working: scheduling disciplines are FlowController implementations (`policy_subsystem_redesign.md`), so an ESA-conformant strict-priority-per-user controller is a registered scheduler type, not a BPA change — and the draft's own "volume-based fair scheduling MAY be used to avoid starvation" is the default's story.
- **Retransmission preference** → the class's routing `table` property: a table preferring reliable CLAs, "if possible" as fallback order, "required but unavailable" resolving to Wait. Its "priorities SHOULD inform CLA parameter selection" is a natural future class-property read at the peer seat.
- **Retention class** → the eviction-rank class property already reserved for the storage-pressure tranche; the TTL tiebreak within a class is the storage layer's existing expiry index.

The fourth, **latest-only delivery, is deliberately not filter material** — and the design says so rather than bending. "Discard if a newer bundle from the same source to the same destination exists" is a cross-bundle query, and "the latest bundle is forwarded when the oldest discarded one would have been" is queue-position inheritance; a Classifier can express neither, because filter output is a pure derivation over one bundle's bytes and the arrival of a *newer* bundle must affect an *older* one already in custody. Latest-only is a queue discipline: a per-flow-replacement FlowController behaviour enabled by an enumerable class property, with flow identity computed at push. It is recorded as a named validation case against the `FlowController` trait shape in `policy_subsystem_redesign.md`.

## Wiring and registration

One immutable, ordered chain per hook, built through the BPA **`Builder`** — not a registry. The component registries (CLA / Service / RoutingAgent / AdminRecord) are lock-guarded dynamic collections because components genuinely come and go at runtime; filters do not, so they get no registry object, no `register_filter` verb on a running BPA, and none of the current engine's lock/swap machinery. The embedder calls `Builder::add_*` methods (per hook and kind; exact naming TBD) before `build()`, and **chain order is call order** — lexically visible in the embedder's construction code. That retires the current name/dependency-resolution machinery (`Error::{AlreadyExists, DependencyNotFound, HasDependants}`); a name argument survives only as a diagnostic label for logs and metrics. `build()` freezes each chain into a plain immutable slice the hot path iterates without synchronisation; hooks nobody extends observe a near-free empty chain. The built-ins are **not** in these chains — they are pipeline code, gated by `Config`, upstream of any registered filter.

Because a closed repository implements these traits without seeing `bpa` internals, the traits and the types they expose are **committed public extension API** — semver surface. The family is three small traits — Verifier, Classifier, and the block-scoped Rewriter — plus a verdict enum, the two-field delta with its registered annotation slots, and the scoped editor handle; the remaining API-shape work is choosing exactly which view of `&Bundle` and the metadata is exposed, and the editor handle's operation set.

## Migration of existing code

- **`BundleValidityFilter`** — deleted. The ingress lifetime/hop check already lives in the pre-drain gate (`gate_reason`); the current filter registration is a documented "cheap, harmless re-check" that stops existing. Originate gets an equivalent inline check for the raw-bytes path.
- **`Rfc9171ValidityFilter`** — dissolved into the checks/gate layer, driven by two BPA `Config` booleans (`primary-block-integrity`, `bundle-age-required`, defaults strict — the existing `filter::rfc9171::Config` fields move, the filter shell goes). Rejection moves from post-store to the pre-drain gate. The `no-rfc9171-autoregister` cfg feature — a compile-time toggle for what is runtime policy — is removed.
- **`ReadFilter`/`WriteFilter`, `WriteResult`, `Mutation`, the `chain.rs` post-rewrite re-validation, and the engine's payload plumbing** — deleted. Filters never receive payload bytes. `WritableMetadata` goes with them, taking `flow_label` (which ships on main today — an opaque tag with no remaining consumer; classification replaces it).
- **The `FilterEngine` registration surface** — names, the dependency graph, and its dynamic-dispatch machinery — replaced by per-hook chains frozen at `Builder::build()`.
- **`update_extension_blocks`** becomes the built-in, fixed head of the ClaSend rewrite stage that registered Rewriters extend. That stage now sits inside the transfer-outcome claim: `forward_bundle` claims the bundle into `ForwardAckPending` before the rewrite and restores the pre-rewrite blocks on synchronous failure — the Egress hooks run inside the claimed window, and the engine returns the bundle alongside any error so the claim resolves CAS-clean.
- **`ipn-legacy-filter`** (settled 2026-08-17): its primary-block rewrite (Ipn→LegacyIpn per next hop) is inexpressible as a Rewriter — the primary block is out of scope by design — and by this document's own taxonomy it is per-hop wire adaptation. It becomes a **config-driven fixed built-in** in the ClaSend rewrite stage (peer-pattern list in bpa `Config`, beside PreviousNode/HopCount); the crate retires or thins to configuration types.
- **`Hook::Ingress` execution** moves from post-store (`ingress_bundle`) to the pre-drain gate; a filter Drop there follows the gate's reporting pattern (reception + deletion reports per flags, §5.6 report-before-dedup preserved on the early-drop path; already-expired remains a silent drop). Egress stays at its ClaSend position; Deliver precedes decrypt.

## Open questions (for review)

1. **The exposed `&Bundle` view.** Settled by [Sketch — the concrete types](#sketch--the-concrete-types): direct `&Bundle` (privacy-on-the-record as the zero-cost projection), no view types (block-body access via the existing bpv7 accessors over `data: &[u8]`), queue items owning `Bundle` by value. The residue is settled too (2026-08-17): a Classifier **sees the deltas applied by preceding links** of the same pass — the engine applies each delta before the next invocation. Invocations additionally carry a `KeySource` (settled: key access stays available; an encrypted block a filter holds no key for is its no-match path).
2. **Drop reporting from Originate/Egress/Deliver Verifiers.** Gate-pattern reporting is defined for Ingress; the other hooks need their drop/report semantics stated (Originate returns an error to the service; Egress/Deliver drop with reason vs delete). Sequencing settled (2026-08-17): Phase 2 keeps today's per-hook behaviour verbatim; the formal semantics land with Phase 3's repositioning.
3. **Scanner component.** Queue/verdict shape: a new registry row + queue wiring — how much lands with the queue architecture work vs later; provenance-chained shape waits on the virtual-CLA re-forward entry point in the routing work.
4. **Rewriter details.** Settled (2026-08-17): the name is **Rewriter**, and the editor handle **refuses** edits to blocks under existing BPSec coverage (no cascade — this node is not their security source). **Deliver-host resolved (2026-08-27): yes.** The Rewriter serves *two* boundaries — Egress (next-hop wire prep, network-scoped) and Deliver (stripping transport-scoped extension blocks — network QoS, custody: "transport headers" vs "content headers" — before a bundle reaches a raw-bundle `Service`). It applies only to the raw-`Service` path (the `Application` path is payload-only). Because it edits extension blocks and never the payload, the Deliver invocation runs *before* the payload's BPSec decrypt and uses the `KeySource` it is passed to decrypt any extension block it must inspect — so there is no acceptor-ordering constraint. Next-hop context is Egress-only and rides `RewriteContext::Egress`. The standard RFC transport blocks (Previous Node/Hop Count/Bundle Age) may be stripped by fixed machinery; the pluggable Deliver Rewriter targets embedder-defined transport blocks, the dual of the ingress Classifier that consumed them. Remaining: the handle's exact operation-set surface, and (C3 wiring) fixed-vs-pluggable split for the standard transport blocks.

## Roadmap

The working task list for these phases is [`refactor_plan.md`](refactor_plan.md).

- **Phase 1** — this draft folds into `filter_subsystem_design.md`; back-references from streaming §5.3 and `queue_architecture.md`. *(doc only)*
- **Phase 2** — the three traits (Verifier, Classifier, Rewriter + its scoped editor handle) + verdict/`MetadataDelta` types and the annotation-slot registration; the `Builder` registration surface and frozen per-hook chains; delete `ReadFilter`/`WriteFilter`/byte-return/re-validation and the `FilterEngine` registration machinery; dissolve validity + rfc9171 into gate + `Config` (cfg feature removed). *Incremental-safe.*
- **Phase 3** — move the Ingress chain onto the pre-drain gate; wire Originate/Egress/Deliver single-pass positions per the matrix; restart re-admission of stored bundles. Early Drop now skips drain + store. *Incremental-safe; rides the existing parse split.*
- **Phase 4** — scanner/verdict component and the virtual-CLA re-forward entry point. *Waits on the queue-architecture and routing/RIB work; build when a consumer exists.*

When the full streaming gate lands (`streaming_pipeline_design.md` §5.4/§5.7) the Ingress pass moves onto the accumulation buffer before any spool opens — the payload-free signatures make that a no-op for filter authors.

## Related documents

- [`routing_table_redesign.md`](routing_table_redesign.md) — routing-key selection, the RIB→FIB compilation, multiple routing tables, inter-table jumps
- [`policy_subsystem_redesign.md`](policy_subsystem_redesign.md) — the `ClassPolicy` definition and properties, FlowControllers and scheduling, configuration, and the centralized policy manager
- [`queue_architecture.md`](queue_architecture.md) — processing blocks, queue assignment (its "Flow labels and classification" section is superseded by [`MetadataDelta` and the traffic class](#metadatadelta-and-the-traffic-class) above)
- [`streaming_pipeline_design.md`](streaming_pipeline_design.md) — §5.2.2 (reject, don't rewrite), §5.3–5.7 (the gate, tee'd ingress), §6.1 (egress seam)
- [`filter_subsystem_design.md`](filter_subsystem_design.md) — the current (to-be-replaced) design
- [`policy_subsystem_design.md`](policy_subsystem_design.md) — the current policy design (to be replaced by the redesign above)
- [`../../docs/acme_design.md`](../../docs/acme_design.md) — the AdminRecord registry (admin-path extension surface)
