# Routing Table Redesign (DRAFT / RFC)

> **Status: draft for review.** Captures the routing-key-selection and multi-topology-routing design settled in the design dialogue of 2026-07-17, split out of the filter-redesign discussion (now folded into [`filter_subsystem_design.md`](filter_subsystem_design.md)). Companion split: the filter tranche commits the classification-delta *seam* (the delta shipped with annotation slots only — `route_key` itself arrives additively with this tranche, `class` with the policy tranche, both non-breaking on the `#[non_exhaustive]` delta); everything in this document is the routing tranche. Once implemented this folds into [`routing_subsystem_design.md`](routing_subsystem_design.md) and the draft is retired. Nothing here is implemented yet.
>
> **Terminology.** This document uses **RIB** for the BPA's routing component (as the code does — `bpa/src/routing`, `rib.find`) and **FIB** for the compiled lookup structure internal to it. The wider "TVR holds the RIB, the BPA holds the FIB" framing lives one level up and is unchanged by this design.

## Motivation

Peer feedback names a real hard-coding: Hardy is destination-based — the RIB lookup key is the destination EID, welded in at `rib.find(&mut bundle)`. Three pressures converge on the same redesign:

- **Not everything routes on destination.** Segment routing (route on a label-stack top), policy routing (route by traffic class), and multi-topology routing all need a lookup key or table that is not simply "the destination".
- **DTN EIDs match badly.** `ipn` EIDs are numeric — RFC 9758's fully-qualified node number is a u64 — but `dtn` EIDs are free-text names, and routing on names via glob patterns ordered by a specificity metric (the DPP draft's approach) imposes a total order on something with no natural hierarchy, and pays for it per bundle.
- **Routes are the churny thing.** TVR contact windows open and close constantly. Today every bundle pays for pattern-plus-specificity evaluation; the churn belongs on a compiled structure, paid per route install.

## The decomposition

Routing is three separable functions, all currently hard-coded to one behaviour each:

```text
key   = select_key(bundle)                default: destination EID
table = select_table(bundle, key)         default: the one table
action = lookup(table, key)               default: glob match + specificity order
```

This design makes each one right: key and table selection become Classifier concerns (metadata-fed, with a default table), and the lookup becomes an internally-compiled, per-scheme LPM walk with explicit inter-table jumps.

## Key selection — the filter seam

The RIB lookup generalises to `route_key.unwrap_or(destination)`, where `route_key: Option<Eid>` is a Classifier-set `MetadataDelta` field (see [`filter_subsystem_design.md`](filter_subsystem_design.md)). This is the Linux fwmark shape: classification feeds routing through metadata; the lookup itself stays fixed machinery, and Dispatch stays hook-free.

The key is per-node-stable (a function of the bundle's wire state, derivable once at the input boundary) and persists across Waiting sweeps as a cache under the policy epoch: when the chain's declared behaviour changes — or the operator forces a reevaluation — the epoch's lazy re-derivation at the Dispatch block re-derives it before the next lookup ([`storage_consistency_updates.md`](storage_consistency_updates.md)), the same freshness story as every other classification cache.

**Segment routing is the worked example** (detail in the filter doc): an input Classifier derives the effective top of a label-stack extension block (skipping segments equal to self, so the RIB never sees key == self and deliver-vs-forward stays keyed on the real destination); an egress Rewriter pops consumed segments onto the wire, committing only by transmission. Forward progress generalises from "re-targeting" to **"the lookup key is consumed"** — BIBE consumes by re-targeting the destination, segment routing consumes by popping.

**Cross-cutting selectors** — patterns like `dtn://*/telemetry/**` that correspond to no name subtree — are not routes: they are classifications. The Classifier maps such traffic to a **virtual class EID** (a *named* forwarding-equivalence class) and routes are installed against that name. Hierarchical selectors go in tables; cross-cutting selectors go in Classifiers. Everything has a home, and the vocabulary stays EIDs throughout.

## The RIB/FIB split — key compilation is an implementation detail

The public vocabulary is EIDs everywhere: RoutingAgents install EID patterns (into a named table), lookups are performed for an EID, and jump actions are defined over EIDs. Internally, the RIB compiles the installed pattern set of each table into a fast FIB — for example interned name-subtrees mapped to u64 subnets over an LPM trie — and derives per-lookup keys itself. This is the classic control-plane/data-plane split every serious router makes; deliberately, it is a **performance hack, not an ecosystem concept** — MPLS-style key spaces are never exposed to RoutingAgents, filters, or operators.

Single ownership is what makes it sound:

- **No skew.** The compile side (pattern → subnets at install time) and the lookup side (EID → key at dispatch time) live in one component, so they cannot disagree — the failure mode that would make an *exposed* key space untenable (every RoutingAgent doing its own translation) is structurally impossible.
- **No operator address plan.** Because the RIB owns both sides, the mapping can be automatic **interning** of route-pattern subtrees, renumbered freely whenever convenient. There is nothing to configure and nothing to misconfigure.
- **Assignment, never hashing.** A hash of an EID destroys prefix aggregation and collides silently — a misroute no one sees. Interning is assignment.
- **`ipn` is the identity mapping.** RFC 9758's fully-qualified node number is already a u64; an ipn sub-FIB compiles to FQNN LPM with allocator ranges as natural subnets, zero interning required.

The performance argument is a churn asymmetry: the name structure is stable while routes churn constantly (TVR contact windows), so paying compilation per route install against a stable structure and one LPM walk per bundle strictly beats per-bundle glob-plus-specificity evaluation. Installable patterns are constrained to the monotonic subset (see [Installable patterns — the monotonic subset](#installable-patterns--the-monotonic-subset)), so the specificity metric retires locally: specificity *is* prefix depth.

Scope boundary: the FIB decides **node-level disposition** only (forward toward / local / drop / wait); service demux on the real destination EID stays where it is, downstream of a "local" verdict — exactly as IP routes to `local` before port demux.

**Union routes flatten at install** (`RouteTable::insert`/`remove` — on main since `6d6d0a31`). A route installed with a pattern set is sugar for one route per member, and selection competes *matches*, not *predicates* — no scalar "set specificity" is meaningful, since any aggregate mis-sorts some member: a broadest-member score lets an unrelated broad member drag a specific sibling behind routes the sibling strictly beats, and a narrowest-member score fails symmetrically. The FIB compiler does the same thing by construction. A set-level score remains valid only as a *predicate breadth* descriptor, never for selection. Under the monotonic subset this also makes selection fully deterministic: two distinct patterns of equal score cannot both match one EID.

## Installable patterns — the monotonic subset

Installable route patterns are restricted to the **Strict Monotonic Subset** defined by DPP (draft-taylor-dtn-dpp, "EID Pattern Constraints (Monotonic Specificity)"): wildcards confined to the leaves of the naming hierarchy, at most one wildcard, no complex globs, and the wildcard — if present — terminal in hierarchy order. Terminality guarantees that any two patterns matching the same EID are nested, which is what makes specificity total (it *is* prefix depth) and LPM exact. The same subset flows end-to-end — DPP exchange → RoutingAgent install → FIB compile — with one specificity notion throughout.

The rationale is the description/lookup split: the generic eid-pattern language exists to *describe* arbitrary EID sets, while the RIB uses patterns as *keys in an ordered lookup*, and the monotonic subset is exactly the class where that is well-defined — the constraint DPP requires for O(1) scoring is the same one FIB compilability requires. This is the CIDR precedent: routing restricted itself to prefixes while ACLs kept the rich match language. The full eid-pattern language accordingly remains available wherever patterns are *predicates* (BPSec `PatternKeySource`, policy matching); the restriction applies only at the route-install API. Cross-cutting selectors were never routes — they are Classifier material (virtual class EIDs) — and unions are simply multiple entries.

Nothing useful is lost at the API: ranges (`ipn:100.[10-13].*`) stay accepted and compile by prefix-splitting, CIDR-style, which keeps lookup deterministic even where installed ranges overlap. Enforcement is type-level — a `RoutePattern` parse type that can only represent the subset, so a non-routable pattern is rejected at parse/install time with a clear error, never discovered at lookup time — the same capability-scoping move as the filter Rewriter's editor handle.

The DPP Harmonized Specificity Score is never evaluated in-node: a lookup key has exactly one scheme, so the match set is single-scheme by construction and cross-scheme comparison cannot arise, even within a mixed-scheme table. The score remains a DPP wire/management-plane uniformity concern (computed locally, never transmitted, per the draft).

## Multiple tables — multi-topology routing

Tables are first-class, and vital for MTR: per-traffic-class contact plans from TVR, security-segregated topologies, emergency overlays. Precedents: IS-IS MT (RFC 5120), Linux's multiple routing tables + RPDB.

**Table selection is a Classifier output, not a class property** *(settled 2026-09-04)*: the table is part of the per-bundle routing decision — the RPDB half of the Linux precedent, which was never a property of the tc class — so the classifier emits `route_table` in its delta exactly as it emits `route_key`, and a class that needs a specific table is simply a classifier that emits both facts together. `ClassPolicy` carries no table: it stays pure treatment (weights, contracts, eviction), which removes the FlowController↔routing weld at the root rather than guarding it. Operator coherence is preserved where the operator sees it: the `[classes]` stanza still declares `table = X` in one place, compiled into the stock classifier's emitted delta rather than into a policy property. Chain order arbitrates (per-field last-writer-wins, bespoke after stock), and bundles for which no classifier expressed an opinion use the default table.

**Both lookup inputs are persisted Classifier outputs under the policy epoch** *(settled 2026-09-04, superseding the 2026-09-03 ephemeral-table interim)*: `route_table` and `route_key` ride the Classification group as caches of the chain's output, re-derived by the epoch's lazy pass ([`storage_consistency_updates.md`](storage_consistency_updates.md)). The lookup resolves its full input entirely from the bundle's record — `route_table.unwrap_or(default)`, `route_key.unwrap_or(destination)`, both already freshened by the Dispatch block's epoch check — and reads no policy: with no `ClassPolicy` table property, the record is the lookup's only input beyond the tables themselves. The accepted trade, named: re-tabling a class is a classification-policy change, reaching parked bundles through the epoch's lazy re-derivation rather than for free at the next sweep — and it is self-announcing, since a re-table moves the stock classifier's declared rule-hash; the right price, because re-tabling is a rare topology-membership event, while the frequent churn (table *contents*, TVR contact windows) is ordinary RIB traffic and unaffected. A persisted `route_table` naming a table that no longer exists resolves to no-match → Waiting, per wait-not-drop and strict isolation — never an uninstalled fall-through.

**Tables are scheme-agnostic: a table is a topology, and a scheme is not a topology.** One real topology — an emergency overlay, a bulk contact plan — legitimately contains routes to both `ipn` and `dtn` destinations, so scheme must not appear in table selection: that would mix MTR with addressing, force every topology into per-scheme table pairs, and make a TVR contact plan split its installs by scheme. Instead, each table is internally scheme-partitioned into per-scheme compiled sub-FIBs (the ipn FQNN trie, the dtn interned trie), dispatched by the lookup key's scheme. This needs no cross-scheme specificity machinery, because **the match set for any lookup is single-scheme by construction** — a key has exactly one scheme, so `ipn` and `dtn` routes in one table never compete. The Linux precedent read correctly is the VRF: the topology object contains per-family FIBs, and the family is an axis *inside* the table, never a selector *of* it. Scheme partitioning of the internal key spaces is thereby solved one level down — per-scheme-within-table, nothing shared.

## Inter-table jumps — the "then this table" action

Fall-through between tables is not engine configuration but an explicit, optional **route action**: an entry whose action is "continue lookup in table X" — the Junos `next-table` / nftables `goto` shape, netfilter user-chains in routing clothes.

- **Strict isolation is the absence of a jump route.** No match in a table → Waiting, per wait-not-drop: a bundle waits for *its* topology's next contact and can never silently leak onto the wrong one. Strictness needs no flag — it is the default behaviour of a table with no jump entries.
- **Fall-through is a visible, auditable RIB entry**, installed by a RoutingAgent like any other route. A leak between security topologies is a reviewable route, not an invisible engine behaviour.
- **Per-prefix granularity for free.** LPM decides whether a lookup hits a specific route or the jump entry, so one table can fall through for some destinations and be strict for others — and overlay topologies are natural (an emergency table holding a handful of overrides plus one jump to main).
- **Semantics: goto (tail-call), never call/return.** In LPM routing the jump entry *is* the decision for its prefix — there is nothing to return to.
- **Loop guard: a per-lookup visited-tables set** — the same guard pattern as the RIB's existing Via-route recursion protection, keyed by table instead of EID. The lookup EID is invariant across gotos, so a table revisit is *definitionally* a loop: detection is exact, with no tuning constant. Loop detected → no-match → Waiting, logged loudly. RIB-internal, so a bitmask over small table ids suffices.
- **Jumps are EID-level.** "Continue lookup for this EID in table X" — a jump preserves the EID and therefore its scheme; the target table's matching sub-FIB is selected by the key's scheme and derives its own internal key. A jump can never change the key's scheme.

## Composition with the wider architecture

- **Licence-clean end to end**: a closed `bpa-server` ships Classifiers (key and table selection, class EIDs), Rewriters (e.g. the SR pop), and RoutingAgents (routes, tables, jumps) against an unmodified `bpa` — no key-space coordination required of it, because there is no public key space.
- **Frozen filter chains** mean key-derivation policy cannot skew mid-process; the **policy epoch's lazy re-derivation** re-derives `route_key` for stored bundles when the operator declares a reevaluation ([`storage_consistency_updates.md`](storage_consistency_updates.md)). Internal FIB renumbering needs no re-admission safety at all, because compiled keys never persist — the vocabulary at every boundary is EIDs.
- **Waiting semantics** compose: strict-table misses, unmapped destinations, and jump-loop detection all resolve to Waiting, never Drop.

## Sequencing

The three pieces are decoupled, and the FIB compilation — being a pure performance hack behind an unchanged lookup contract — can land **last**, benchmarked against the matcher it replaces:

1. The classification-delta seam in the filter tranche (Phase 2 ships the delta with slots only); `route_key` joins the delta with this tranche's first step, `route_table` with its tables step (the only cross-tranche commitments — both are Classifier-emitted routing inputs, neither is a class property).
2. Tables, selection policy, and the jump action on top of the *existing* pattern-matching lookup (mechanism first, representation later).
3. The compiled FIB (interning + LPM) as a drop-in replacement for the per-table matcher.

## Open questions

1. **Table identity.** Names vs small numeric ids at the API; who creates tables (config-declared vs RoutingAgent-declared on first install); lifecycle of an emptied table.
2. **Selection-policy shape.** The `[classes]` config format — match rules, per-class treatment properties, and the per-stanza `table` the stock classifier emits — and the default-table fallback. Also `TableId` stability: a persisted `route_table` must survive policy recompiles, which argues for stable names/ids over snapshot indices (shared with open question 1).
3. **Sweep granularity.** Which table's route change sweeps which Waiting bundles (conservative: any change sweeps all, as today; refinement is an optimisation).
4. **RoutingAgent API.** The table parameter on install; representation of the jump action; whether Via routes and jumps interact (a Via resolving through a route in another table).

## Related documents

- [`filter_subsystem_design.md`](filter_subsystem_design.md) — the `MetadataDelta` (`class` + `route_key`), Classifier/Rewriter kinds, the segment-routing worked example, restart re-admission
- [`policy_subsystem_redesign.md`](policy_subsystem_redesign.md) — the `ClassPolicy` owning the `table` property; multi-topology tables as per-intent topologies; the centralized policy manager
- [`storage_consistency_updates.md`](storage_consistency_updates.md) — the durable policy epoch and operator-declared reevaluation that keep the persisted `route_key` cache honest
- [`routing_subsystem_design.md`](routing_subsystem_design.md) — the current routing design and this draft's eventual home
- [`queue_architecture.md`](queue_architecture.md) — processing blocks and queues; Waiting/gated queues (its flow-label section is superseded by the filter doc's `MetadataDelta`)
- The DPP draft (`/workspace/dpp/draft-taylor-dtn-dpp.md`) — defines the Strict Monotonic Subset adopted for installable patterns; its Harmonized Specificity Score remains a wire/management-plane concern, unused in-node
