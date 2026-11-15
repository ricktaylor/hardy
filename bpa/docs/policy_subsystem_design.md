# Policy Subsystem Design

This document describes the BPA's egress policy subsystem: the per-peer queues a routed bundle waits in, the FlowController that assigns each bundle a queue and releases it onto the CLA's lanes, and the storage-backed channels beneath those queues.

## Related Documents

- **[Routing Design](routing_subsystem_design.md)**: peer selection and route change handling.
- **[Filter Subsystem Design](filter_subsystem_design.md)**: traffic-class classification, and the Egress chain run at transmission.
- **[Storage Subsystem Design](storage_subsystem_design.md)**: bundle persistence and crash recovery.
- **[Deferred CLA Transfer Outcomes](design.md#deferred-cla-transfer-outcomes)**: the `Accepted` answer and the `ForwardAckPending` hold.
- **[Policy Subsystem Redesign](policy_subsystem_redesign.md)** (draft): the planned evolution of this subsystem.

## Design Goals

- **Mechanism in the open crate, policy behind a trait.** Routing decides *where* a bundle goes; the policy decides *when* and *in what order* it leaves. An embedder, closed-source included, supplies a scheduler through `FlowControllerFactory` against the unmodified `bpa` crate.
- **Zero configuration is a working node.** A CLA registered without a policy gets the null policy: one FIFO queue per peer, no shaping.
- **Waiting costs storage, not memory.** A full queue spills to the store rather than blocking the sender or dropping anything.
- **The CLA stays policy-free.** A CLA declares how many transfers it can carry in parallel and never learns about priorities or classes.

## Architecture Overview

```
  Dispatcher: RIB::find() → DispatchAction::Forward { peer, next_hop }
        │
        │ ClaRegistry::forward(peer, next_hop, bundle)
        ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Peer (one per CLA adjacency)                                         │
│                                                                      │
│   controller.queue_for() → queue in 0..queue_count()                 │
│   (out of range → queue 0, logged)                                   │
│   status := ForwardPending { peer, queue, next_hop }                 │
│                                                                      │
│   ┌───────────┐   ┌───────────┐          ┌─────────────┐             │
│   │  queue 0  │   │  queue 1  │   ...    │  queue M-1  │  hybrid     │
│   └─────┬─────┘   └─────┬─────┘          └──────┬──────┘  channels   │
└─────────┼───────────────┼───────────────────────┼────────────────────┘
          │ one poller per queue, sharing the peer's lanes:
          │ controller.forward(queue, bundle) per free lane
          ▼
┌──────────────────────────────────────────────────────────────────────┐
│ FlowController (one per peer)                                        │
│                                                                      │
│   decides when to transmit, and under which lane directive:          │
│   EgressQueueSet { next_free → lane None, pinned[i] → lane Some(i) } │
└──────────────────────────────────┬───────────────────────────────────┘
                                   │ EgressQueue::forward(bundle)
                                   ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Dispatcher::forward_bundle                                           │
│                                                                      │
│   1. claim ForwardPending → ForwardAckPending (duplicates lose)      │
│   2. load the stored bundle; its headers become resident             │
│   3. update extension blocks (Previous Node, Hop Count, Bundle Age)  │
│   4. Egress chain (Rewriters, then Verifiers)                        │
│   5. Cla::forward(lane, cla_addr, bundle_id, total_len, stream)      │
└──────────────────────────────────────────────────────────────────────┘
```

The subsystem is three traits in `bpa/src/policy/mod.rs`. **`FlowControllerFactory`** is the policy bound to a CLA: it declares the policy's queue count and builds one `FlowController` per peer. **`FlowController`** assigns each bundle a queue (`queue_for()`) and, called by that queue's poller (`forward(queue, bundle)`), decides when the bundle leaves and on which lane directive. **`EgressQueue`** is the transmission endpoint for one lane directive.

Two counts meet in the controller: the policy's **queue count** (M, `queue_count()`) counts places where bundles wait, and the CLA's **lanes** (N, `ClaInit::lanes`) count transfers in flight. The controller maps one onto the other, so a policy works unchanged over any CLA. A third bound is the BPA's own: the peer keeps at most its effective lanes of forwards in flight across all its queues, which the egress seat cap bounds ([below](#one-controller-per-peer-draining-into-lanes)).

`queue_for()` takes no per-bundle input, so every bundle takes the controller's own assignment. The classification input — the traffic class assigned by the Classifier chains ([Filter Subsystem Design](filter_subsystem_design.md#metadatadelta-and-the-traffic-class)) — remains in scope and is designed in the [redesign](policy_subsystem_redesign.md).

## Key Design Decisions

### Queues hold waiting work; lanes carry in-flight work

A routed bundle needs somewhere to wait until the link can take it. That place is per peer, so a slow or dark link holds back only its own traffic, and it is where ordering and rate decisions are made without involving routing or the CLA.

Lanes are the CLA's side. As the `Lanes` rustdoc (`bpa/src/lanes/mod.rs`) defines them for a CLA, lanes are parallel transport channels — QUIC streams, DSCP classes, separate TCP connections — carrying in-flight transfers with no priority among them: the policy decides what goes onto each lane, the CLA transmits what arrives, and one lane's in-flight transfer must not head-of-line block another's.

`Lanes` is an enum: `Serial` (one transfer at a time, the default for a CLA that declares nothing), `Limited(n)` declaring explicit lanes `0..n` (zero is unrepresentable), or `Unlimited` (an effectively unconstrained CLA, such as a datagram CL). It is part of the set-once `ClaInit` snapshotted at registration, so a CLA whose parallelism changes re-registers. The controller reaches the lanes through its `EgressQueueSet`: a required `next_free` queue that transmits with lane `None`, letting the CLA pick its next free lane, and one `pinned` queue per explicit lane of a `Limited` CLA, up to `lanes::MAX_PINNED_LANES` (256). A CLA may declare more lanes, which the BPA logs at registration: a policy then pins to only the first 256, while forwards on the next free lane still reach every lane.

### A total queue model

`queue_count()` returns a `NonZeroU32`, so queue 0 always exists; `queue_for()` returns a plain `u32`, not an `Option`; and `Peer::forward` clamps an out-of-range index — a policy bug — to queue 0 and logs it. Every bundle has a real queue, and there is no special-cased default queue.

The persisted status requires this. A queued bundle's status is `ForwardPending { peer, queue, next_hop }`, and the queue's channel recovers spilled bundles by that `(peer, queue)` identity (`BundleStatus::same_queue`), so every assignment must name a channel that exists; clamping rather than rejecting keeps a buggy policy from stranding the bundle. `Option` appears only at the lane directive, where `None` means something. Queue indices carry no fixed priority: relative priority is the policy's decision, and index 0 is guaranteed only as the clamp target.

### One controller per peer, draining into lanes

`Peer::start` builds the controller from the CLA's factory, then one hybrid channel and one poller per policy queue, and only then publishes the peer, so a reachable peer always has a working controller. The controller owns all per-peer scheduler state for the peer's lifetime; on peer removal the channels close, the pollers exit once drained and their in-flight forwards have finished, and the controller is dropped.

The pollers drain their queues into the peer's **effective lanes**, from the CLA's declared `Lanes` and the BPA's egress seat cap (`BpaBuilder::egress_lane_cap`, default 8): one for `Serial`, which a CLA that declares nothing gets (`ClaInit::default()`, or a gRPC CLA sending no `lanes`); the declared count bounded by the cap for `Limited`; and the cap for `Unlimited`. The lanes are a budget of permits shared by all of the peer's queue pollers, so the CLA's declaration holds whatever the policy's queue count. A poller takes a lane before it pulls a bundle, and the bundle's `controller.forward(queue, bundle)` runs as a task of its own on the registry's task pool, holding the lane until `EgressQueue::forward` returns — when the CLA answers `Cla::forward`. While every lane is busy the pollers pull nothing more; a poller whose queue is empty holds no lane while it waits, so an idle queue leaves the peer's lanes to its busy ones. The cap exists whatever the declaration, because each in-flight forward holds its loaded bundle until the CLA answers: a peer's worst case is the cap times the largest bundle in memory. A cap of 1 forwards to each peer serially, one bundle at a time in queue order. The default is above one so that a CLA declaring several lanes, as TCPCLv4 does, has concurrent offers out of the box: an offer streams the whole bundle to the CLA, so one offer at a time caps a peer at one bundle per offer however fast the link. A CLA that declares nothing keeps serial offers, since it may not tolerate overlapping forwards completing out of order.

Offers start in queue order and may complete out of order. Bundles carry no ordering guarantee, and nothing downstream depends on per-peer completion order: each offer claims its own bundle out of `ForwardPending` and resolves only that claim, and the peer sweeps (`reset_peer_queue`, `reset_peer_ack_pending`) are status-conditioned, so an in-flight offer neither blocks nor is undone by them. A peer removed mid-offer resets up to its lanes of claimed bundles from `ForwardAckPending` to `Waiting`, each re-sent at the next routing event — as it reset the one in-flight offer before lanes, and as it resets every accepted transfer. An offer whose claim is swept while its data is still loading is never made: `forward_bundle` re-checks the claim once the load returns.

Both pacing levers follow, per lane. A controller that rate-limits by awaiting before it transmits holds its lane; once every lane is held, the channel fills and further bundles spill to storage — shaping neither drops bundles nor grows memory. Up to the peer's lanes of such awaits run at once, so a per-call delay paces each lane and the aggregate rate is the per-lane rate times the lanes; a controller enforcing a peer-wide rate shares one budget (a token bucket) across its calls. A CLA paces the BPA by when it answers: answering `Accepted` early frees the lane while the acknowledgement is in flight, and a CLA at capacity withholds its answers, so the pollers stop pulling once every lane awaits one.

### Hybrid Channel Architecture

Each policy queue is a hybrid memory/storage channel (`bpa/src/storage/channel.rs`) buffering `poll_channel_depth` bundles (default 16). A conditional swap durably moves the bundle's status to the queue's status before the bundle is offered to the buffer, which makes the in-memory copy disposable: on the fast path it reaches the poller in memory, and when the buffer is full it is dropped and the channel's background poller recovers the bundle from metadata storage.

```
    ┌──────────┐  buffer full   ┌──────────┐  storage drained,   ┌──────────┐
    │   Open   │ ──────────────►│ Draining │  buffer ≤ cap / 2   │   Open   │
    └──────────┘                └────┬─────┘ ───────────────────►└──────────┘
                                     │ send while draining
                                     ▼
                                ┌───────────┐
                                │ Congested │ ──► poller drains again at once
                                └───────────┘

    Any state ──► Closing (channel closed)
```

Memory stays bounded per queue whatever the arrival rate, and a full queue never blocks or fails the sender. Every send pays the status write; the fast path saves the storage re-read. Delivery is at-least-once, so `forward_bundle` claims each bundle with a conditional swap to `ForwardAckPending` before loading its data and offering it: a duplicate copy loses the swap, at the cost of one metadata compare-and-swap, never a read of the bundle.

#### Hysteresis and congestion signalling

The poller re-opens the fast path only when a drain cycle finds nothing more in storage to push, the buffer holds at most half its capacity (`buffered <= cap / 2`, inclusive so a capacity-1 channel can re-open), and the `Draining` → `Open` swap succeeds. The threshold is hysteresis: re-opening at the first free slot would flip paths on nearly every send near capacity, each flip costing a storage scan.

`Congested` is the senders' signal to the poller. A sender that spills during a drain moves `Draining` to `Congested`, so the re-open swap fails and the poller drains again at once, rather than re-opening the fast path where new arrivals would overtake the spilled bundle.

### Factories are code; configuration binds them

A `FlowControllerFactory` is code linked in by the embedder; configuration only selects among the factories a build contains. Following the Linux `tc` model, it is bound per CLA — a queuing discipline attached to an interface — and instantiated per peer.

- **At build time**, `BpaBuilder::cla(name, cla, policy, init)` binds a configured CLA's factory. bpa-server resolves each CLA's `policy` key against its named `policies` table; it compiles in no policy types, so every entry is ignored with a warning and a CLA naming one fails startup.
- **At runtime**, `BpaRegistration::register_cla(name, cla, policy, init)` takes the factory from the caller, so an in-process CLA brings its own.
- **With no factory**, the registry binds the null policy. A CLA registered over gRPC always takes this path, since the wire carries no policy.

This registration shape is interim, and the model is still being designed. The intended one registers policies by name once, when the BPA is constructed, and has each CLA register with a named reference to one — the shape bpa-server's configuration already has, with its named `policies` table — so the binding stays configuration, and a CLA registered over gRPC is no longer confined to the null policy ([Policy Subsystem Redesign](policy_subsystem_redesign.md)).

The null policy (`policy::null_policy`), the only factory `bpa` ships, declares one queue and hands every bundle to `next_free` without rate limiting. It leaves the pinned queues idle on purpose: applying no policy means imposing no lane constraint, so a multi-lane CLA spreads transfers across whichever lanes are free.

### Failure evidence is scoped, and every exit returns to routing

The CLA's answer to `Cla::forward` decides how much of the peer's queue a failure disturbs:

- **`Sent`** completes the forward: the bundle is reported forwarded where requested, and deleted.
- **`Accepted`** holds the bundle in `ForwardAckPending` until the CLA reports the outcome; a deferred `Failed` re-enters dispatch for that bundle alone.
- **`NoNeighbour`** is link-scoped evidence: `Store::reset_peer_queue` returns all the peer's `ForwardPending` bundles to `Waiting`, along with the offered one.
- **An error** is bundle-scoped evidence: only the offered bundle returns to `Waiting`, with no inline retry, because a synchronous failure can be deterministic and a retry would spin.

Peer removal and CLA unregistration close the peer's channels first, so no new send can land, then withdraw its RIB entries, which resets its queued bundles and its unresolved accepted transfers to `Waiting`. Peer ids do not survive a restart, so recovery resets `ForwardPending` and `ForwardAckPending` bundles to `Waiting`. A `Waiting` bundle is re-dispatched on the next routing event, possibly to a different peer ([Routing Design: Route Change Handling](routing_subsystem_design.md#route-change-handling)).

## Integration

- **Routing** hands the policy a peer through `ClaRegistry::forward`, which returns the bundle if the peer has vanished, and the dispatcher parks it in `Waiting`. Peer selection, ECMP included, is complete before the policy sees the bundle.
- **Filters** run the Egress chain inside `forward_bundle` on every transmission attempt, in memory only, so a re-routed bundle is re-filtered with fresh context.
- **Storage** provides the queue channels, the peer sweeps, and restart recovery; status semantics are in the `BundleStatus` rustdoc (`bpa/src/bundle/status.rs`), recovery in [Storage Subsystem Design](storage_subsystem_design.md#crash-recovery).
- **gRPC** carries the `lanes` declaration at registration (the shared `lanes.Lanes` message: absent is serial, and a `limited` 0 is rejected) and a lane on each forward, but no policy.

## Standards Compliance

RFC 9171 §5.4 Step 4 makes "the time at which the BPA invokes CLA services" a BPA implementation matter — the latitude the FlowController occupies — and requires the Bundle Age increase "at the last possible moment before the CLA initiates conveyance": the extension-block update runs after the controller releases the bundle, so the age it writes includes time spent queued.

## Planned Work

This document describes what is built. The next tranche — the traffic-class input, a class-aware `push(bundle, class)` in place of `forward(queue, bundle)`, controller-owned queues from the queue mechanism ([Queue Architecture](queue_architecture.md)), the scheduling discipline, and adaptive de-staging of the hybrid channel — is designed in [Policy Subsystem Redesign](policy_subsystem_redesign.md), which folds into this document as it lands.

## Testing

- [Unit Test Plan](unit_test_plan.md) — §3.3 egress policy, §3.7 the hybrid channel state machine, and §3.8 CLA registry and peer logic.
- [CLA Integration Test Plan](cla_integration_test_plan.md) — Suite B forwarding and Suite D peer management.
