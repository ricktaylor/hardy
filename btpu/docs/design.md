# hardy-btpu Sender/Receiver Design (tranche 2)

Design for the second tranche of work on the high-level `Sender`/`Receiver` layer of `hardy-btpu`, turning the initial implementation into the protocol engine for real convergence layers. The codec and transfer-window layers from the initial contribution are unchanged by this design.

## Design Goals

The initial crate implements [draft-ietf-dtn-btpu](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/) correctly at the wire level, but its `Sender`/`Receiver` layer assumes a single deployment shape: fixed-size link PDUs, lossless-enough links, whole bundles in memory. This design generalises that layer to serve three concrete consumers without per-link forks:

- **Constant-bit-rate framed links** (CCSDS-style): fixed-length PDUs, padding mandatory, blind repetition for loss protection.
- **Ethernet**: variable-length frames with a 46-octet minimum payload, padding wasteful beyond the minimum, blind repetition for loss protection.
- **QUIC datagrams** ([QUBICLE](https://datatracker.ietf.org/doc/draft-ek-dtn-qubicle/) unreliable service, [RFC 9221](https://www.rfc-editor.org/rfc/rfc9221.html)): self-delimiting datagrams where padding actively wastes congestion-window budget, and where the usable PDU size changes during a connection.

A fourth goal comes from inside Hardy rather than from a link type: the BPA is moving to a streaming bundle pipeline (`Sink::dispatch_streamed`, storage `stream_out()`), and this layer must bound its memory use the same way — no full-bundle buffering on either the send or receive path.

Two non-goals, stated explicitly because both were considered and rejected: this layer does not provide reliability (see the repetition decision below), and it does not couple to `hardy-bpa`'s streaming traits (see the integration section). The crate remains `no_std` + alloc, sans-io, runtime-free.

## Architecture Overview

The CLA is a demand-driven pump between two pull interfaces:

```text
  ingress                                      egress
  link ──► CLA ──► dispatch_streamed           storage stream ──► CLA ──► link
            │        (BPA pulls segments)                 │
            ▼                                             ▼
        btpu Receiver                                btpu Sender
        (emits in-order chunks                       (next_pdu(max_len),
         as the prefix extends)                       segments at pack time)
```

On egress, the link pulls: the CLA calls `next_pdu(max_len)` when the link reports send capacity (a QUIC datagram slot, a frame interval, a socket becoming writable), passing the capacity actually available *now*. On ingress, the BPA pulls: the CLA feeds received PDUs to the `Receiver`, which emits reassembled bundle data as a stream of in-order chunks that the CLA forwards into `dispatch_streamed`, backpressured by the BPA's bounded channel. Neither direction holds a complete bundle in memory.

## Key Design Decisions

### Pack-time segmentation with per-call PDU capacity

The initial implementation fixes `pdu_size` at `Sender` construction and cuts a bundle into segments eagerly inside `enqueue()`. That is the wrong binding time for two of the three target links: the QUIC datagram limit derives from the negotiated `max_datagram_frame_size` *and* the live path MTU, and can shrink mid-connection (path migration, PMTU discovery), stranding already-cut segments that no longer fit. It also forces whole-bundle ingestion, which conflicts with the streaming pipeline.

Instead, `enqueue` records the bundle (or accepts it as a chunk stream) and segmentation happens in `next_pdu(max_len)`, cutting segments against the capacity offered on each call. The configured `pdu_size` becomes an upper bound and the default for callers with genuinely fixed frames.

One constraint makes this subtler than it looks: [BTP-U §6](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/) requires any repeated Message to be an exact copy of an already emitted Message. Re-cutting a segment at a different boundary on a repeat pass would violate this. The first emission pass therefore pins each segment's byte offsets, and later passes re-cut at exactly those boundaries even if the offered capacity has changed since. Offsets are cheap to retain (a `u64` pair per segment); the segment *bytes* are not retained — see the repetition decision.

### Padding is a policy, not a behaviour

The initial `next_pdu` unconditionally pads to the full PDU size, which is correct for exactly one of the three link types. Each consumer wants a different rule, so padding becomes configuration: pad to the full PDU size (constant-bit-rate links, today's behaviour), pad only up to a minimum length (Ethernet's 46-octet floor — although in practice the NIC's own zero-fill decodes as BTP-U indefinite padding, so even this is belt-and-braces), or no padding at all (QUIC datagrams, where every padding byte spends congestion-window budget that reliable streams on the same connection are competing for).

### Priority interleaving replaces the single FIFO

[BTP-U §4.1](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/) permits interleaving Transfer Messages from different Transfers precisely so a large low-priority bundle cannot block a small urgent one. The initial implementation queues all of a bundle's segments contiguously in one FIFO, which makes head-of-line blocking structural. This also mismatches Hardy's forwarding model, where `Cla::forward(queue, ...)` already expresses per-bundle queue lanes that the CLA currently has nowhere to put.

The replacement is per-transfer queues with a scheduler: the packer fills each PDU by pulling from the highest-priority transfer with pending messages, round-robining within a priority class. Unsegmented Bundle messages and Transfer Cancels join the scheduler as single-message pseudo-transfers so that ordering and priority apply uniformly. A useful second-order effect: when repetition is configured, round-robin scheduling naturally spreads a message's repeats across different PDUs rather than emitting them back-to-back, which is strictly better protection against bursty frame loss at no extra cost.

### Repetition is the only loss mechanism — acknowledgement feedback was rejected

An earlier draft of this design proposed an emission ledger: retain emitted messages, consume per-datagram acknowledgement/loss reports from the QUIC stack (RFC 9221 datagram frames are ack-eliciting), and re-emit exactly the messages from lost PDUs — selective repeat instead of blind repetition.

This was rejected as building QUIC inside QUIC. QUBICLE deliberately offers both services on one connection: a bundle whose delivery matters belongs on the reliable stream service, where QUIC's loss recovery is real and mature. Ack-driven repair in the CLA reconstructs a worse ARQ one layer up — heuristic loss declaration, roughly an RTT of repair latency (by which time data on an intentionally-unreliable flow is often stale), and retention buffers that conflict with the bounded-memory goal. There is also a semantic trap: an RFC 9221 acknowledgement confirms the *packet* carrying the datagram arrived, not that anything consumed it.

What survives is deliberately smaller: the blind repetition count is the single loss knob, set per-enqueue, and the CLA may tune it from *aggregate* link statistics — QUIC lost-packet counters, Ethernet driver stats. BTP-U §6 explicitly anticipates link-layer signalling triggering increased repetition, so this is tuning a protocol-native parameter with a statistic, not acknowledgement-driven reliability. The protocol-native escalation beyond repetition, for deployments where the multi-segment completeness cliff bites, is the [FEC extension](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/) — not acknowledgements.

A pleasant consequence of the rejection: messages need to be retained only until their last *scheduled* emission, after which nothing is kept. Combined with pack-time segmentation, a large transfer's unsent remainder lives in the storage stream rather than in the Sender, and repeat passes re-pull chunks (re-cutting at the pinned offsets) rather than holding segments across passes.

### The window releases itself

The initial API requires the CLA to call `complete(transfer_number)` to free a window slot, but on a unidirectional link there is no acknowledgement to anchor that call to, and `next_pdu` returns an opaque buffer, so the CLA cannot even tell when a transfer's messages have finished leaving the queue. In practice the method's argument was ignored and any call released *some* slot — an API the caller cannot use correctly.

Instead, the Sender releases a transfer's slot when the last scheduled emission of its Transfer End is packed, and `next_pdu`'s return value reports which transfers drained in that PDU so the CLA can surface completion upward. `complete()` is removed rather than repaired; `cancel()` remains, is a no-op for unknown transfer numbers, and frees a slot only for a transfer that was actually active.

### The Receiver streams the contiguous prefix

The initial Receiver buffers every segment of a transfer until completion, then concatenates them into a fresh buffer — unbounded memory under adversarial or just unlucky traffic, plus a full copy of every bundle. The replacement emits reassembled data incrementally: each time the in-order prefix of a transfer extends, the newly contiguous segments (already zero-copy slices of their PDUs) are emitted as chunks, with the final chunk marked as such. A gap in the segment sequence simply stalls emission until repetition fills it.

This shape exists because of where the data goes next: it maps one-to-one onto the BPA's `Segment::Next(Bytes)` / `Final(Bytes)` ingress stream, whose `Final`-carries-data form happens to mirror Transfer End carrying the final segment. Memory is bounded to out-of-order segments beyond the contiguous prefix — the receiver-side resource-exhaustion concern largely dissolves as a side effect of the architecture rather than needing a dedicated cap, though a configurable ceiling on buffered out-of-order bytes remains as defence in depth. The Bundle Length hint ([§9.1](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/)) shifts role accordingly: from sizing a reassembly preallocation to early-rejecting oversized transfers and advising the BPA's spool.

## Integration

The crate stays free of `hardy-bpa` dependencies. The shapes above are designed to *rhyme* with the BPA's streaming seams — `next_pdu(max_len)` answers the link's demand the way `dispatch_streamed` answers the BPA's; the Receiver's chunk emission feeds `Segment::Next/Final` through a trivial adapter loop in the CLA — but the coupling lives entirely in the CLA crate that bridges them. This preserves `no_std` portability and keeps the protocol library testable without an async runtime.

Egress integration is staged: today's `Cla::forward(queue, cla_addr, bundle: Bytes)` delivers whole bundles, and a chunk-fed `enqueue` accepts that as a single chunk. When BPA egress streaming lands, the same `enqueue` accepts the storage stream directly and the pull-through pipeline (storage → Sender → link) completes without further API change — pack-time segmentation is the piece that makes this possible. The `queue` parameter of `forward` maps directly onto the Sender's per-enqueue priority.

## Standards Compliance

- [draft-ietf-dtn-btpu](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/) — §4.1 (interleaving), §5 (transfer window), §6 (repetition, exact-copy rule), §9.1 (Bundle Length hint). Note: the implementation's window-validity check intentionally follows the §5 prose over the published Figure 2 pseudocode, which mis-classifies a repeat of the greatest transfer number as new; a correction to Figure 2 is queued for the next draft revision.
- [draft-ietf-dtn-btpu-fec](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/) — message framing only; FEC schemes remain out of scope for this tranche.
- [RFC 9221](https://www.rfc-editor.org/rfc/rfc9221.html) — consumed via QUBICLE; this design deliberately uses no per-datagram acknowledgement signals (see the repetition decision).

## Testing

The existing unit and integration tests carry over as the baseline: full-size padding with a repetition count of one reproduces the initial implementation's behaviour exactly. New coverage required by this design: pack-time segmentation under varying per-call capacity (including capacity shrinking mid-transfer, verifying pinned segment boundaries on repeat passes), scheduler fairness and priority ordering under interleaving, automatic window release, and contiguous-prefix emission under reordered and duplicated segment arrival. A crate test plan will follow the Hardy test-plan format once the tranche is scoped into PRs.
