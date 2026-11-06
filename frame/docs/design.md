# hardy-frame Design

A common pair of traits for the frame services below Hardy's frame-based convergence layers: a transmit half and a receive half, stackable, with backpressure in both directions.

## Design Goals

Several convergence layers Hardy plans have the same shape. A protocol engine turns bundles into frames and frames back into bundles, and below it sits a stack of frame services: LTP over Encapsulation Packets or Space Packets over AOS, BTP-U over an AOS virtual channel, BTP-U over raw Ethernet or QUIC datagrams, any of them over UDP for testing. Without a common interface each engine binds each link directly, every new link means new code in every engine, and nothing can be tested over an emulated link with light-time delay, loss and scheduled outages.

The goals, in the order they shaped the traits:

- **Layers compose.** Each layer is a crate that depends on this one and on no other layer, takes the halves of whatever is below it, and offers halves of its own above. Swapping a modem for UDP, Ethernet or an emulator changes no code above it.
- **Backpressure end to end, in both directions.** No layer grows an unbounded queue: a full link stops the transmitter, and a consumer that stops reading stops the link.
- **Nothing packed without room.** A packer such as BTP-U cannot take back bytes it has packed, so it must hold room for a frame before it packs one.
- **No copy per layer.** A packer writes its frame straight into the memory its permit lends, which can be a ring slot or a DMA buffer the link owns; each layer below adds its header in place in that same memory; and a received frame can be a view of a lower layer's buffer.
- **Runtime-agnostic and `no_std`.** The crate depends only on `core`, `alloc`, `bytes`, `futures-core` and `thiserror`, never on tokio or `hardy_async`.
- **Static dispatch.** A stack is a type, so no call through the traits is dynamic and no future is boxed.
- **Deterministic tests.** Every contract is observable without a clock.

In scope are frame services: services that carry bounded, whole-or-nothing units, each delivered entire or not at all. Byte streams such as TCPCLv4 are out of scope, and so is the CLA contract with the BPA, which is unchanged: the traits layer frames below the engines and stop there. A link whose per-frame budget cannot afford an asynchronous call, such as a shared-memory frame ring at 100 Gbit/s, keeps a direct path from its engine to the ring and may offer the traits as a facade for tests; since a permit can lend a ring slot, that facade still writes frames in place.

## Architecture Overview

Each frame service is two halves, a transmit half (`FrameTx`) and a receive half (`FrameRx`), and there are three kinds of crate around them. Link crates sit at the bottom and only provide halves: UDP, raw Ethernet, a QUIC connection's datagrams, a modem driver, a ground station's SLE services. Layer crates both consume and provide: Encapsulation Packets, Space Packets, AOS and an impairment layer take the halves below as constructor arguments and offer halves of their own above. Engine crates sit at the top and only consume: the LTP CLA and the BTP-U CLAs built on `hardy-btpu`, which implement Hardy's CLA trait on one side and consume frame halves on the other.

A stack is a type, assembled by construction in the deployment profile, the only code that names more than one layer: `Ltp<Epp<Aos<Modem>>>` for a CCSDS link, `Ltp<Udp>` against another implementation. Each layer checks at construction that the service below satisfies what it needs, so an impossible stack fails when it is assembled rather than at its first frame.

The model is RINA-like: one interface, repeated at every layer, with the mechanism fixed by the traits (reserve, send, pull) and the policy set by each layer (its queue depth, its release timer, hold or drop on a full channel). It stops short of RINA's flow allocation by name: a CLA is built from a concrete stack whose channel keys (an APID, a virtual channel, a protocol ID) are configuration, which keeps each CLA's stack explicit in one place.

## Key Design Decisions

### Separate transmit and receive halves

Transmit and receive are separate traits, and a layer may offer one without the other. Real links are often asymmetric: a forward and a return path with different protocols, frame lengths and availability, or no return path at all. A unidirectional link is then a stack with one half, and an engine that needs a return path takes any number of receive halves from whichever routes reach it. Where the two directions of one link are coupled, the coupling lives inside that layer's implementation, not in the traits.

### Frame size is a property of each half, fixed for its life

A half states the range of frame lengths it carries as a `FrameSize`: every frame is from a minimum to a maximum number of octets, and a frame carries at least one byte. A fixed service has equal bounds (AOS transfer frames); a maximum service has a minimum of 1 (UDP datagrams); and some services have a real minimum, such as Ethernet's 46-octet payload, below which the MAC pads with bytes the receiver cannot tell from data, so the layer above pads to the minimum itself with padding it can tell apart. A service accepts any frame within its range and refuses one outside it, rather than padding it.

A layer reads the size of the half below once, at construction, and derives its own from it with `behind`, which shrinks both bounds by the layer's header; a stack is checked with `covers`, which says whether one range lies within another. Configuration never states two sizes independently, because a fixed service under an engine that frames to a maximum emits short frames. The size never changes during a half's life: a size change means a new half, built by the stack code, as a new QUIC session or a reconfigured virtual channel is anyway. A size a half could publish changes to was rejected, because every layer above would then re-derive its sizes mid-stream with frames already packed against the old size.

The size is a two-field struct with private fields and `Option`-returning constructors rather than a public range, so that a zero-length or inverted range cannot be built and the arithmetic behind a header cannot underflow. A public `RangeInclusive` would allow range literals, at the price of an invariant nothing enforces.

### Transmit is two-phase: reserve, then send

Transmitting a frame takes two steps. `poll_reserve` waits for room for one frame and returns a `Permit` lending the memory the frame is written into; it is the only wait, and waiting is backpressure. It is a poll method, with `reserve` the same wait as a future, so that an engine owning a half can be a plain state machine polled from one task, with no future of its own to store, no lock and no spawn; `hardy-btpu`'s frame sender is one, and a layer over another half delegates the poll. It waits while the queue below is full or the link is down, and never fails. `Permit::send` consumes the permit with the frame's length, never waits, and returns whether a layer below accepted the frame. The single-phase `FrameTx::send` does both for a caller that already has its frame, and refuses a frame outside the half's size before it waits for room.

The split exists for packers. BTP-U packs a PDU only once it holds a permit, so a packed PDU always has somewhere to go, and cancellation keeps acting on everything not yet packed. With a single-phase send, a packer must either pack first and risk holding a frame with nowhere to put it, or guess the room below.

The contract is small. Admission is first come, first served, and frames leave in the order their permits are sent, because an engine's control segments must not overtake its data. Dropping a reservation before it is granted holds nothing, and dropping a permit unsent returns its room and sends nothing. While the link is down, reservations wait, which pauses every packer above with no policy of its own; a half that has closed for good never grants again, so its user learns of the closure from its receive stream ending or from its own shutdown. Failing every send on a closed half instead would set a drain loop spinning.

This is the shape of tower's `Service::poll_ready` and `call`. The traits keep their own names because tower-service needs `std`, its `Service` has nowhere to put a frame size, its `&mut self` does not fit halves shared between layers, and a `Service` that keeps its reservation between `poll_ready` and `call` holds a permit across an `.await`, which the next decision rules out.

### A permit is used at once, and is not `Send`

A caller reserves, writes and sends with no `.await` while it holds the permit, so that no layer below ever waits on a caller that has gone off to wait for something else while holding room. The traits enforce what they can: `Permit` carries no `Send` bound, a permit that holds room in a queue carries a `PhantomData<*const ()>`, and a permit that wraps one inherits that, so in a task that must be `Send`, as Hardy's tasks on tokio's multi-threaded runtime are, holding a permit across an `.await` is a compile error (a `compile_fail` doctest on `Permit` pins it). A layer whose reservation needs other waits does them before it reserves below, and a layer that turns one frame into several queues the frame and feeds the pieces down from a task of its own.

### The permit lends the frame's memory, and headers go in place

A permit is `AsMut<[u8]>`: a slice exactly as long as the longest frame the service carries, whose contents are unspecified, so the caller writes every byte it sends, padding included. The memory is whatever the bottom of the stack has. A layer that adds a header wraps the permit below and lends the slice behind its header; when the frame is sent it writes the header in front, now that the length is known, and sends the permit below with the header's length added. Every header in a stack therefore lands in front of the payload in the bottom's own memory, with nothing copied and no headroom for any half to report. A header whose length depends on the frame lends the slice behind its longest form, and either always writes that form or moves the payload forward to meet a shorter one, which is one copy within the slice.

Lending a `BytesMut` instead was rejected: it can only be heap memory, so a ring or DMA link would always copy once at the bottom. A slice reaches that memory directly.

### Sending means accepted below, not transmitted

A successful send means a layer below has accepted the frame into its queue. The traits carry no completion: a frame lost after acceptance is lost, as fire-and-forget means, and each binding says where it accepts a frame. A completion that resolved on transmission was tried and cost a one-shot channel per frame and code in every layer to complete the frames above from the frames below; unreliable engines need nothing more than acceptance, and confirmation of transmission is deferred, not ruled out.

### Receive is a pull stream

The receive half is a `Stream` of frames that its consumer pulls. Frames arrive in link order; a frame is consumed only when the poll returns it, so dropping a pending pull loses nothing; the end of the stream means the service has closed for good; and a consumer that stops polling stops taking frames, which is receive backpressure. Receive has no equivalent of the irrevocable pack that makes transmit two-phase, so it is a plain stream, and the asymmetry between the halves is deliberate.

Pull gives the consumer what a push design takes away: no allocation per frame, batching for free (a consumer drains while frames are ready and hands over at the first pending poll), cancel-safe selection beside a timer or a shutdown signal, and exclusive ownership of the half, which a single-owner state machine such as `hardy-btpu`'s receiver wants. A sink bound to the half and called per frame was rejected for those reasons, and a rule that receive never blocks was rejected because holding is what carries backpressure down to a bottom that can hold, such as a ring, where a filling buffer is the only congestion signal the far side has. Loss is not signalled through the traits: a consumer that cares detects it from its own sequence numbers and timers, as LTP and BTP-U do.

A received frame may be a view of a lower layer's buffer; a consumer that keeps a frame's bytes past its next poll copies them, or accepts that it pins the buffer behind them.

### Errors

A send fails in one of two ways: `Dropped`, a transient refusal of one frame by a layer's own policy, or the loss of its link after granting the reservation, after which the service carries on; and `BadLength`, a frame outside the service's size, the one misuse a send can see now that channels are chosen when the stack is built. Both are plain values that allocate nothing. Closing is not an error. The layer that sees a failure classifies it, and layers above pass it up unchanged. Receive has no errors: a stream yields frames until it ends.

### What is deliberately left out

Demultiplexing is concrete, not a trait: a layer that carries several channels hands out per-channel halves from constructors keyed as that layer's protocol keys them, because the keys differ in kind across layers and stack assembly is concrete code that knows them. Layers never spawn: a layer that needs a task returns it as a future from its constructor and the deployment profile runs it, on whichever runtime it has. Link state is not published: while the link is down reservations wait, and a half that has closed never grants again, so there is nothing to subscribe to. The transmit half is `Send + Sync`, since stacks share it between tasks, and is implemented for `&T` and `Arc<T>`; the receive half has no bound of its own and is implemented for `&mut R`, as `Stream` is.

## Integration

The crate needs nothing from `hardy_async`. Channel-backed halves, which most layers would build on, need a bounded channel whose senders can reserve before they send; they are separate crates, as are the layers and links. `hardy-btpu` is a sans-IO protocol library and stays free of these traits in its core; behind its `frame` feature it offers a sender over any `FrameTx` and a receiver over any `FrameRx`, each a type per stack (`Sender<AosTx>`, `Receiver<UdpRx>`), which take their framing from the half's size at construction and drive the two-phase send themselves: wait for a packable PDU, poll for a permit, pack into the permit's slice, send with the length returned. The CLA contract with the BPA is unchanged: a BTP-U CLA, or LTP green, answers `Sent` once its frames have been accepted below.

## Standards Compliance

The crate implements no protocol. The services it describes are those of CCSDS 732.0-B-4 (AOS), 133.0-B-2 (Space Packets) and 133.1-B-3 (Encapsulation Packets), IEEE 802.3 Ethernet payloads, RFC 9221 QUIC DATAGRAM frames, and UDP datagrams; each binding's crate records how it maps.

## Testing

Tests of the public API live under `tests/`: `size` for the size arithmetic a stack is assembled with, `tx` for the transmit contract over an in-memory one-slot link and a length-prefix header layer stacked on it (a frame copied into the slot and delivered, a frame outside the size refused before any wait, a permit lending the longest frame and sending a prefix, a permit dropped unsent returning its room, a held permit making the next reservation wait, a closed half never granting, a refused frame leaving the half in service, shared halves forwarding to the half they share, and a header written in front of the payload in the bottom's own memory), and `rx` for the receive stream's order and end. Every future is hand-polled with a no-op waker, with no runtime and no clock. The `compile_fail` doctest on `Permit` pins that a permit cannot be held across an `.await` in a `Send` task.
