# hardy-btpu Design

A `no_std` protocol library for the Bundle Transfer Protocol - Unidirectional (BTP-U): the wire codec, the transfer window, and a `Sender`/`Receiver` pair that a convergence-layer crate drives against a frame-based link.

This document describes the crate as it is. Direction that has been sketched but not built is confined to [Next steps](#next-steps-not-yet-implemented-or-agreed-in-detail) at the end, and nothing above that heading depends on it.

## Design Goals

- A pure protocol library. The crate is `#![no_std]` + `alloc`, sans-io, and depends on neither `hardy-bpa`, `hardy-bpv7`, nor an async runtime. BTP-U's target links include CCSDS frames and broadcast radio, where the surrounding software may be embedded or a different runtime entirely, so the protocol engine must be usable from any of them and testable without one. The cost is that the crate never performs I/O and never drives itself: a convergence-layer crate pumps PDUs in and out.
- Faults split by trust boundary. Everything decoded from the wire is untrusted, so processing it surfaces events, never errors. `Result` is reserved for trusted local operations, meaning constructor validation and `enqueue`. A hostile or corrupt PDU therefore cannot make the receiver "fail", only report what it dropped.
- Memory bounded by default. A unidirectional receiver gets no say in what a peer sends, so every buffer a remote peer can grow has a cap that is on unless the operator explicitly chooses otherwise. The send side bounds its own queue the same way.
- Zero-copy where the data flows. Received PDUs are `Bytes`, and every segment the codec yields is a slice of the PDU it arrived in. Copies happen only where a consumer needs contiguous bytes.
- Link-agnostic sending. The sender knows only the PDU size and the link's framing discipline (fixed-size frames or variable-length PDUs). It does not know whether the link is UDP, Ethernet, or a CCSDS virtual channel.

## Architecture Overview

The crate has three layers plus an optional adapter. Each layer is usable without the ones above it, so a CLA with unusual needs can stop at the codec or the window primitives and build the rest itself.

```text
   CLA crate (async, owns the socket or frame device)
      │ enqueue(bundle, opts)        ▲ ReceiverEvent::BundleReceived, ...
      ▼                              │
   sender::Sender                 receiver::Receiver      ── high-level engine
      │ next_pdu()                   ▲ receive_pdu(Bytes)
      ▼                              │
   transfer::TransferNumberAllocator / TransferWindow   ── Section 5 window
      │                              ▲
   codec::{encode_message, pad_pdu} / codec::decode_pdu ── wire format
      │                              ▲
      └────────────── link PDUs ─────┘
```

- `codec` encodes and decodes messages, hints, and headers, and classifies bare bundle frames on shared links. Its decode path is a lazy iterator over one PDU.
- `transfer` holds the two Section 5 window primitives: the receiver's sliding window and the sender's transfer-number allocator.
- `sender` and `receiver` are the engine a typical CLA uses: segmentation and PDU packing on one side, reassembly and window bookkeeping on the other. `fec` carries the FEC extension's message types and the `FecScheme` trait the engine is written against.
- The `tower` feature wraps `Sender` and `Receiver` as `tower::Service`s and exposes the sender's PDU drain as a `Stream`, with waker-based backpressure. It is a thin adapter over the same `&mut self` methods, not a second engine.

The CLA drives both engines. On egress it calls `enqueue` as the BPA forwards bundles and `next_pdu` whenever the link can take a frame. On ingress it hands each received PDU to `receive_pdu` and acts on the returned events. Both engines are single-owner `&mut self` state machines with no interior locking.

## Key Design Decisions

### Receive-side faults are events, not errors

`Receiver::receive_pdu` is infallible. Every decode fault and every policy disposition is a `ReceiverEvent` appended to the same list as the bundles that were delivered, so a fault late in a PDU never discards the events produced by the well-formed prefix before it. The alternative, a `Result` that fails the whole PDU, was rejected because a PDU legitimately carries messages from several transfers, and one bad message must not cost the others their delivery.

Fault containment inside the codec has two tiers, following the self-framing property of the Section 7 header and the skip-and-continue rule of Section 7.3. When a message's extent is known from its header length but its interior is malformed, `decode_pdu` yields an error for that message and resumes at the next boundary, and the receiver reports `MalformedMessage`. When the framing itself fails, because the header is truncated, the length runs past the buffer, or a bundle-reserved type byte appears mid-stream, no further boundary can be found, so the iterator stops and the receiver reports `MalformedPdu` as the last event of that PDU. The mid-stream case is an encapsulated bundle in its native format; Section 7.3 says a receiver that cannot determine its extent must not process the remainder, and this crate does not parse bundle formats. `MessageIter::is_exhausted` lets the receiver tell the two apart.

Benign dispositions are data rather than faults. A message outside the window, a repeat of a cancelled or rejected transfer, a Cancel for an unknown transfer (Section 8.4), an empty segment, a message that contradicts the transfer's established segment sequence, or an FEC message on a core transfer each produce `MessageDropped` with a `DropReason`, and the caller decides whether to count, log, or ignore them.

### Configuration is validated newtypes, not config structs

`PduSize`, `WindowSize`, `MaxBundleSize`, and `SendQueueDepth` each enforce their range in `TryFrom`, so an invalid value is a typed error at the edge and no constructor panics. Each range carries a guarantee: a `PduSize` spans one message header to the 20-bit content-length ceiling, which is exactly the guarantee `next_pdu` needs that anything `enqueue` accepted can be drained; a `WindowSize` is the Section 5 range; a `MaxBundleSize` has no "unlimited" value because an unbounded reassembly buffer is a memory-exhaustion lever handed to the peer, so an operator who wants no limit says `usize::MAX` explicitly. Under the `serde` feature the newtypes serialize as plain integers and re-validate on deserialize, so a configuration file cannot smuggle in an out-of-range value.

### The window is enforced on span, not count

Section 5 forbids the sender from emitting any message whose transfer number is at or below the greatest emitted minus the window size. Because numbers are allocated sequentially, that is a bound on the span of outstanding numbers, not on how many there are: if transfer 3 completes before 0, admitting 4 would push 0 out of the receiver's window even though only one transfer is active, and a reordered End for 0 would then be dropped on arrival. The allocator therefore keeps outstanding numbers in allocation order and refuses to allocate while the oldest would fall out of range. Allocation order, not numeric order, survives the 2^32 roll-over.

The allocator is the single owner of the outstanding set. `Sender::complete` and `Sender::cancel` act only on numbers it reports as outstanding, so a duplicate or bogus call cannot free a slot that was never allocated, and a Cancel is queued only for a transfer that actually exists.

### Segmentation happens at enqueue time against a fixed PDU size

`Sender::enqueue` decides immediately how a bundle travels. One that fits in a PDU, hints included, becomes a single Bundle Message and takes no window slot. Otherwise the bundle is cut into Transfer Segment messages and a Transfer End against the configured `PduSize`, a transfer number is allocated, and every message is pushed onto one FIFO queue. `next_pdu` packs that queue in order until the next message would not fit. Sizing is checked before the transfer number is taken, so a PDU too small to carry a segment never disturbs the number sequence.

This is the simplest design that satisfies the exact-copy rule of Section 6 (a repeated message must be byte-identical to one already emitted) and it is correct for links whose PDU size is fixed for the life of the sender. It has three limits: it requires the whole bundle in memory at `enqueue`, it binds segment boundaries to a PDU size that some links renegotiate, and a single FIFO means a large bundle's segments are emitted contiguously ahead of anything queued behind them. Those limits are the subject of [Next steps](#next-steps-not-yet-implemented-or-agreed-in-detail); nothing in the current API promises otherwise.

Caller-supplied hints ride the Bundle Message or the first segment, since hints are transfer-scoped (Section 7.2). The Bundle Length hint (Section 9.1) is always derived by the sender and a caller-supplied one is discarded, because the receiver uses it to reject oversized transfers early and the sender is the only party that knows the truthful value. An empty bundle is refused at `enqueue`: a Bundle Message's content must be a valid bundle (Section 8.1), and an empty one would also be the one message no minimum-size PDU could ever drain.

### Link framing is configuration, and bare bundles go through the sender

BTP-U links differ in whether a PDU must be a fixed size. `LinkFraming::FixedSize`, the default, pads every PDU to the `PduSize` with Definite Padding, which is what CCSDS-style frames need. `LinkFraming::Variable` leaves PDUs unpadded and treats the `PduSize` as a ceiling, which is what a datagram link wants.

A variable-length link may additionally be shared with peers that speak raw bundles, and BTP-U reserves the message-type values that collide with a bundle's first byte (Section 12.1) precisely so a receiver can tell the two apart. `BundleFraming::Bare` lets the sender emit a fitting, hint-free bundle as its own bytes with no BTP-U header. This happens inside the sender's queue, not around it. A bare frame is queued behind whatever preceded it, counts against the `SendQueueDepth`, and is visible to whatever schedules that queue. Writing bare bundles to the link directly would let them race and starve the transfers the sender is pacing, and would hide them from any future prioritisation.

### Receiver memory is bounded by construction

The receiver holds every segment of an in-progress transfer until it completes, so the cap on bundle size is the cap on memory, and it is enforced as data arrives rather than after reassembly. A transfer is rejected the moment its accumulated bytes exceed `MaxBundleSize`, or earlier if the sender's Bundle Length hint already promises an oversized bundle. Zero-length segments are never stored, which keeps every map entry at least one byte and therefore bounds the number of entries by the same cap; the draft says such segments should not be sent (Section 8.2), so dropping them is a local policy the specification permits.

Two further structures could otherwise grow without limit. Cancelled and rejected transfers must not be resurrected by a late or repeated segment (Section 4.2), so their numbers are remembered with the reason, but that map is pruned by the same window advance that expires transfers and is therefore bounded by the window size. Hints are kept one per hint type with the latest value winning, which is structurally bounded by the hint-type space and the one-byte value length.

Messages that contradict a transfer's established segment sequence, such as a second End that disagrees with the recorded final index or a segment beyond it, are dropped rather than applied, because applying them would make completion permanently unsatisfiable and pin the transfer's memory until the window expires it. A bogus End at exactly the highest index seen is undetectable at this layer; that is a job for bundle-level integrity.

### Reassembly copies once and shares when it can

A completed multi-segment transfer is concatenated into one contiguous buffer. The copy is deliberate: the BPA parses bundles from contiguous bytes, and one copy per delivered bundle is cheap relative to the transfer. A single-segment transfer and an unsegmented Bundle Message hand back the segment's own `Bytes`, which is a reference-count increment, not a copy. Segments themselves are slices of their PDU from decode to delivery.

### Enum extensibility follows the trust boundary

Enums decoded from the wire (`Message`, `HintItem`) carry a faithful `Unknown` catch-all and are otherwise exhaustive. Unknown message types and unknown hints relay byte-exact, and `MessageFlags` keeps the three reserved bits rather than discarding them, because the Message Flags registry (Section 12.3) may assign them and a relay that zeroed them would corrupt a future extension in flight. The FEC messages carry a single opaque payload for the same reason: the internal boundaries are scheme-defined, so decode followed by encode is the identity even with no scheme registered.

Enums this crate produces (`ReceiverEvent`, `DropReason`, the error types) are plain exhaustive with no `#[non_exhaustive]`. A consumer that matches per variant should get a compile error when one is added; within the workspace that break is the checklist that new dispositions are handled.

### The tower adapter stays thin and single-owner

The `tower` feature implements `Service<SendRequest>` (and a `Service<Bytes>` convenience) for `Sender`, an infallible `Service<Bytes>` for `Receiver`, and `Stream<Item = BytesMut>` for the sender's PDU drain. `poll_ready` is the sender's admission gate, and it applies the same two predicates `enqueue` does: a window slot must be allocatable and the pending queue must be below its `SendQueueDepth`. Unsegmented bundles take no window slot, so without the depth bound the queue would grow without limit whenever the drain side is slower.

Wake-ups run in both directions from single-slot wakers: draining a PDU wakes a parked `poll_ready`, and enqueueing or cancelling wakes a parked `poll_next`. The sender is a perpetual source and `poll_next` never yields `Ready(None)`. Because the `Service` half, the `Stream` half, and `complete`/`cancel` all need the same `Sender`, it is single-owner by contract: sharing means `Arc<Mutex<_>>`, and `tower::buffer::Buffer` is documented as unsuitable because it moves the sender into a worker task and strands the drain and the completion calls.

### FEC is framing only

The FEC extension's four message types are encoded, decoded, and tracked so that a core transfer and an FEC transfer with the same number are detected as mixing and the FEC message dropped. The `FecScheme` trait fixes the shape a pluggable scheme must have, following the FECFRAME framework, but no scheme is implemented and the receiver does not attempt FEC reassembly. This keeps the wire format complete and the window bookkeeping correct in the presence of FEC traffic without committing to a scheme before one is needed.

## Integration

A convergence-layer crate owns the link and the async runtime and composes this crate with `hardy-bpa`. On egress it turns the BPA's forwarded bundle into a `SendRequest`, calls `enqueue`, and drives `next_pdu` (or polls the `Stream`) as the link accepts frames; for segmented transfers it calls `complete` or `cancel` when it has decided the transfer is finished, since a unidirectional link offers no acknowledgement to anchor that call. On ingress it hands every received frame, BTP-U PDU or bare bundle alike, to `receive_pdu` and dispatches each `BundleReceived` to the BPA, treating the remaining events as telemetry.

The crate deliberately has no `hardy-bpa` dependency, so the BPA's `Cla` and `Sink` traits, its queue semantics, and its streaming pipeline are all bridged in the CLA crate. This preserves `no_std` portability and lets the protocol engine be tested without a runtime. There is no BTP-U CLA in this workspace yet; the first one will be the first real test of the interface.

## Standards Compliance

- [draft-ietf-dtn-btpu](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/): message format, hints, and unrecognized-message handling (Section 7, 7.1, 7.2, 7.3), message definitions (Section 8), the Bundle Length hint (Section 9.1), cancellation semantics (Section 4.2, 8.4), the transfer window (Section 5), the exact-copy rule for repetition (Section 6), and the reserved message-type and flag registries (Section 12.1, 12.3). The target revision is pinned once, in the crate-level rustdoc.
- The receiver's window check follows the Section 5 Figure 2 pseudocode, including its guard that a repeated message for the greatest transfer number is in progress rather than new; without that guard every repeat would re-trigger window expiry.
- [draft-ietf-dtn-btpu-fec](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/): message framing only; no FEC scheme is implemented. The draft's four message types have no IANA-assigned codes yet (TBD1 to TBD4), so the crate uses provisional values from the Private Use range of the message-type registry; they will change when codes are assigned.
- Local policies the drafts permit but do not require: empty Transfer Segments are dropped rather than stored, an empty End still records the final index but stores nothing, and a mandatory bundle-size cap rejects transfers early on the Bundle Length hint.

## Testing

There is no separate test plan for this crate yet. Tests that read private state (window arithmetic, allocator ordering, reassembly bookkeeping) are inline in `sender`, `receiver`, and `transfer`; tests of the public API live under `tests/`, one file per codec subject plus the tower adapter. Every waker path in the tower adapter has a lost-wake-up regression test, and the codec tests pin the byte-exact relay of unknown messages and flag bits.

## Next steps (not yet implemented, or agreed in detail)

Everything below is direction, not description. It records a sketched second tranche of work on the `Sender`/`Receiver` layer so the reasoning is not lost, and it has been discussed but not adopted as a plan: the shapes may change once a real CLA exercises them, and none of the current API is deprecated by it. The codec and transfer-window layers are not expected to change.

### The links that motivate it

The current engine assumes one deployment shape: fixed-size link PDUs, a PDU size known at construction, and whole bundles in memory. Three concrete consumers stretch that in different directions.

- Constant-bit-rate framed links (CCSDS-style): fixed-length PDUs, padding mandatory, blind repetition for loss protection. The current design serves this shape as is.
- Ethernet: variable-length frames with a 46-octet minimum payload, so padding beyond the minimum is wasted; blind repetition for loss protection. `LinkFraming::Variable` covers the unpadded case today; a "pad only up to a floor" option does not exist, though in practice the NIC's own zero-fill decodes as Indefinite Padding, so the floor is belt-and-braces.
- QUIC datagrams ([QUBICLE](https://datatracker.ietf.org/doc/draft-ek-dtn-qubicle/) unreliable service, [RFC 9221](https://www.rfc-editor.org/rfc/rfc9221.html)): self-delimiting datagrams where every padding byte spends congestion-window budget, and where the usable PDU size derives from the negotiated `max_datagram_frame_size` and the live path MTU and can shrink mid-connection.

A fourth pressure comes from inside Hardy: the BPA is moving to a streaming bundle pipeline (`Sink::dispatch_streamed`, storage `stream_out()`), and a CLA built on this crate would want to bound its memory the same way, with no full-bundle buffering on either path.

### Pack-time segmentation with per-call PDU capacity

Proposed: `enqueue` records the bundle (or accepts it as a chunk stream) and segmentation moves to `next_pdu(max_len)`, cutting segments against the capacity the link offers on each call. The configured `PduSize` becomes an upper bound and the default for callers with genuinely fixed frames. This is what a shrinking QUIC datagram limit needs, since segments cut at `enqueue` against a larger size are stranded when the limit drops, and it is what removes the whole-bundle-in-memory requirement.

The exact-copy rule of Section 6 constrains this: a repeat re-cut at a different boundary would violate it. The first emission pass would pin each segment's byte offsets, and later passes re-cut at exactly those boundaries even if the offered capacity has changed. Offsets are cheap to retain; segment bytes would not be.

### Padding as a policy with three settings

Proposed: pad to the full PDU size (today's `FixedSize`), pad only up to a minimum length (the Ethernet floor), or no padding (today's `Variable`). The middle setting is the only addition.

### Priority interleaving in place of the single FIFO

Section 4.1 permits interleaving Transfer Messages from different transfers precisely so a large low-priority bundle cannot block a small urgent one; the current single FIFO makes that head-of-line blocking structural, and it mismatches Hardy's forwarding model, where `Cla::forward(queue, ...)` already expresses per-bundle queue lanes the CLA currently has nowhere to put. Proposed: per-transfer queues behind a scheduler that fills each PDU from the highest-priority transfer with pending messages, round-robining within a class, with unsegmented Bundle Messages and Cancels joining as single-message pseudo-transfers so ordering and priority apply uniformly. A second-order benefit is that round-robin naturally spreads a message's repeats across PDUs rather than emitting them back-to-back. Any such scheduler must still respect the Section 5 span rule at emit time; the FIFO is what makes emit order equal allocation order today.

### Repetition as the only loss mechanism

An earlier sketch proposed an emission ledger: retain emitted messages, consume per-datagram acknowledgement or loss reports from the QUIC stack (RFC 9221 datagram frames are ack-eliciting), and re-emit exactly the messages from lost PDUs. That was rejected as building QUIC inside QUIC: QUBICLE deliberately offers a reliable stream service on the same connection for bundles whose delivery matters, and ack-driven repair in the CLA reconstructs a worse ARQ one layer up, with heuristic loss declaration, roughly an RTT of repair latency, and retention buffers that conflict with the bounded-memory goal. An RFC 9221 acknowledgement also confirms only that the packet arrived, not that anything consumed it.

What would survive is smaller: a blind repetition count as the single loss knob, set per `enqueue`, which the CLA may tune from aggregate link statistics. Section 6 explicitly anticipates link-layer signalling triggering increased repetition, so that is tuning a protocol-native parameter, not acknowledgement-driven reliability. The protocol-native escalation beyond repetition is the FEC extension. One consequence of the rejection is that messages need be retained only until their last scheduled emission.

### A self-releasing window

On a unidirectional link there is no acknowledgement to anchor `complete()` to, and `next_pdu` returns an opaque buffer, so the CLA cannot tell when a transfer's messages have finished leaving the queue. Proposed: the sender releases a transfer's slot when the last scheduled emission of its Transfer End is packed, and `next_pdu` reports which transfers drained in that PDU. `complete()` would be removed rather than repaired; `cancel()` would remain.

### A receiver that streams the contiguous prefix

Proposed: instead of holding every segment until completion and concatenating once, the receiver emits reassembled data incrementally, each time the in-order prefix of a transfer extends, as zero-copy chunks with the final one marked. This maps one-to-one onto the BPA's `Segment::Next(Bytes)` / `Final(Bytes)` ingress stream, and it shrinks the receiver's memory to the out-of-order segments beyond the prefix. The Bundle Length hint would shift from bounding a reassembly buffer to advising the BPA's spool, with a configurable ceiling on buffered out-of-order bytes kept as defence in depth.

### What would not change

The crate would remain free of `hardy-bpa` dependencies: the shapes above are designed to line up with the BPA's streaming seams, but the coupling lives entirely in the CLA crate that bridges them. Egress integration would be staged, since today's `Cla::forward` delivers whole bundles and a chunk-fed `enqueue` accepts that as a single chunk. The existing tests would carry over as the baseline, because full-size padding with a repetition count of one reproduces today's behaviour exactly; new coverage would target pack-time segmentation under varying and shrinking capacity, scheduler fairness, automatic window release, and prefix emission under reordering and duplication.
