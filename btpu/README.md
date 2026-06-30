# hardy-btpu

A pure Rust implementation of the **Bundle Transfer Protocol - Unidirectional
(BTP-U)** as defined in
[draft-ietf-dtn-btpu](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/),
with framework support for the FEC extension defined in
[draft-ietf-dtn-btpu-fec](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/).

## What is BTP-U?

BTP-U is a protocol for the unidirectional, unreliable transfer of binary
objects (typically BPv7 bundles) over frame-based convergence layer protocols.  It sits
between the Bundle Protocol and a convergence layer protocol, providing:

- **Segmentation** of bundles that exceed the convergence layer PDU size
- **Transfer windowing** for managing multiple concurrent transfers
- **Message interleaving** within a single PDU
- **Repetition-based loss protection** (receivers silently accept duplicate
  segments)
- **Forward Error Correction** via a pluggable FEC scheme trait

BTP-U does not require IP services or a return channel, making it suitable for
unidirectional links such as broadcast radio or one-way satellite links.

```text
+----------------------+
|  DTN Application     |
+----------------------+
|  BPv7 / BPv6         |
+----------------------+
|  BTP-U               |  <-- this crate
+----------------------+
|  CL Protocol         |
+----------------------+
```

## Design

This crate is a **pure protocol library**.  It has no dependency on `hardy-bpa`,
`hardy-bpv7`, `std`, or any async runtime.  The crate is `#![no_std]` and only
uses `alloc`, so it is suitable for embedded and CCSDS-frame deployments as
well as hosted ones.  A convergence-layer-specific CLA would use this library
alongside `hardy-bpa` to integrate with the Bundle Protocol Agent.

All buffer operations use the `bytes` crate for zero-copy efficiency.

## Architecture

Three layers of abstraction are provided, from lowest to highest:

### Low-level codec

Direct PDU encoding and decoding:

- `decode_pdu(pdu)` -- parse all messages from a received PDU
- `encode_message(message, buf)` -- encode a single message into a buffer
- `pad_pdu(buf, target_len)` -- pad a buffer to the exact PDU size
- `encoded_message_len(msg)` -- predict the encoded size of a message

### Transfer management

State tracking primitives for building custom sender/receiver logic:

- `TransferWindow` -- receiver-side sliding window that classifies incoming
  transfer numbers as `New`, `InProgress`, or `OutsideWindow`, and detects
  expired transfers
- `TransferNumberAllocator` -- sender-side monotonic allocator that enforces
  window capacity and handles u32 wraparound

### High-level Sender / Receiver

Ready-to-use abstractions for typical CLA implementations:

- **`Sender`** -- accepts bundles via `enqueue()`, automatically segments large
  bundles, packs multiple messages per PDU, and emits padded PDU buffers via
  `next_pdu()`.  Supports transfer cancellation and window management.

- **`Receiver`** -- accepts raw PDUs via `receive_pdu()`, manages the transfer
  window, reassembles segments (including out-of-order delivery), and emits
  `ReceiverEvent`s:
  - `BundleReceived(Bytes)` -- a complete bundle is ready
  - `TransferCancelled { transfer_number }` -- the sender cancelled a transfer
  - `TransferExpired { transfer_number }` -- a transfer fell out of the window

## Wire format

Each BTP-U message begins with a 4-byte header:

```text
 0               1               2               3
 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|  Message Type |H|   Flags   |         Content Length          |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

- **Message Type** (8 bits): identifies the message kind
- **H** (1 bit): hint flag -- when set, hint items follow the message content
- **Flags** (3 bits): reserved
- **Content Length** (20 bits): payload size (max 1,048,575 bytes)

### Message types

| Type | Value | Description |
|------|-------|-------------|
| Indefinite Padding | 0x00 | Zero bytes consumed to end of PDU |
| Definite Padding | 0x01 | Explicit-length padding |
| Bundle | 0x02 | Complete bundle in a single message |
| Transfer Segment | 0x03 | Non-final segment of a segmented transfer |
| Transfer End | 0x04 | Final segment of a segmented transfer |
| Transfer Cancel | 0x05 | Abort a transfer |
| Pre-Agreed FEC Source | 0x70 | FEC source ADU (pre-configured scheme) |
| Explicit FEC Source | 0x71 | FEC source ADU (inline scheme info) |
| Pre-Agreed FEC Repair | 0x72 | FEC repair symbols (pre-configured) |
| Explicit FEC Repair | 0x73 | FEC repair symbols (inline scheme info) |

Message types 0x06 (reserved to avoid collision with BPv6) and 0x80-0x9F
(reserved for the BPv7 CBOR array encoding) are not valid BTP-U message types.

### Shared links: BTP-U PDUs and bare bundle frames

The reservation lets a single link carry both BTP-U PDUs and bare bundle
frames unambiguously. The decoder handles this transparently: when a frame's
first byte is in one of the bundle-reserved ranges, `decode_pdu` returns it
as a single `Message::Bundle` containing the frame bytes verbatim. The CLA
therefore needs no special routing — every received frame goes through the
same `receive_pdu` call, and the resulting `BundleReceived` events look
identical whether they came from a bare bundle frame or a reassembled
multi-segment BTP-U transfer:

```rust
for event in receiver.receive_pdu(&frame)? {
    if let ReceiverEvent::BundleReceived(bundle) = event {
        sink.dispatch(bundle, peer_node, peer_addr).await?;
    }
}
```

A reserved byte encountered *mid-PDU* (after at least one BTP-U message has
been parsed) still errors with `Error::ReservedMessageType`, since a
well-formed PDU cannot contain those bytes mid-stream.

For callers that want to peek at the frame's protocol without going through
the decoder — e.g. to drive per-protocol metrics — `frame_kind()` returns
the classification directly:

```rust
use hardy_btpu::{frame_kind, FrameKind};

match frame_kind(&frame) {
    FrameKind::BtpuPdu    => metrics.btpu_pdus.inc(),
    FrameKind::Bpv6Bundle => metrics.bpv6_bundles.inc(),
    FrameKind::Bpv7Bundle => metrics.bpv7_bundles.inc(),
}
```

The classifier is total: every possible first-byte value falls into exactly
one variant. Unallocated BTP-U byte ranges classify as `BtpuPdu` (a current
decoder parses them as `Message::Unknown` for forward compatibility).

## FEC extension

The `fec` module provides message types and the `FecScheme` trait for pluggable
Forward Error Correction following the FECFRAME framework (RFC 6363).
Implementors provide `encode_source()`, `generate_repair()`, and `decode()`
methods for a specific FEC algorithm (e.g., Reed-Solomon, RaptorQ).

No concrete FEC scheme implementations are included; these would be provided by
a CLA crate or a separate library.

## Configuration

Both `Sender` and `Receiver` accept configuration structs:

```rust
// Sender
SenderConfig {
    pdu_size: 1500,      // convergence layer PDU size in bytes
    window_size: 16,     // transfer window (4..=4096)
}

// Receiver
ReceiverConfig {
    window_size: 16,            // transfer window (4..=4096)
    max_bundle_size: None,      // optional size limit
}
```

## Features

- **`serde`** — `Serialize`/`Deserialize` for the configuration types.
- **`rand`** — adds `Sender::from_rng` and `TransferNumberAllocator::from_rng`
  constructors that seed the initial transfer number from a
  `rand_core::RngCore`. Without this feature, callers pass an explicit
  `initial_transfer_number: u32` (e.g. from a hardware RNG read at boot).
- **`tower`** — implements `tower::Service<Bytes>` for `Sender` (enqueue) and
  `Receiver` (PDU dispatch), plus `futures_core::Stream<Item = BytesMut>` for
  `Sender` (outgoing PDU drain). See *Tower integration* below. Requires
  `std` at the consumer level (the `tower` crate itself is std-only).

## Usage example

```rust
use bytes::Bytes;
use hardy_btpu::{Sender, SenderConfig, Receiver, ReceiverConfig, ReceiverEvent};

// Sender side: `initial_transfer_number` should be unpredictable per the spec
// (e.g. from a hardware RNG). With the `rand` feature, use
// `Sender::from_rng(config, &mut my_rng)` instead.
let initial_transfer_number = read_hw_rng();
let mut sender = Sender::new(
    SenderConfig {
        pdu_size: 1500,
        window_size: 16,
    },
    initial_transfer_number,
);
sender.enqueue(Bytes::from(bundle_data)).unwrap();

while sender.has_pending() {
    let pdu = sender.next_pdu().unwrap();
    transmit_over_link(&pdu);
}

// Receiver side
let mut receiver = Receiver::new(ReceiverConfig::default());

for pdu in received_pdus {
    for event in receiver.receive_pdu(&pdu).unwrap() {
        match event {
            ReceiverEvent::BundleReceived(bundle) => {
                // Pass reassembled bundle to the BPA
            }
            ReceiverEvent::TransferCancelled { transfer_number } => {
                // Sender cancelled this transfer
            }
            ReceiverEvent::TransferExpired { transfer_number } => {
                // Transfer fell out of the receive window
            }
        }
    }
}
```

## Tower integration

With `--features tower`, `Sender` and `Receiver` implement
[`tower::Service`](https://docs.rs/tower/latest/tower/trait.Service.html) so they
plug directly into a `tower::ServiceBuilder` stack (rate limiting, concurrency
limiting, timeouts, metrics, tracing). `Sender` additionally implements
[`futures_core::Stream`](https://docs.rs/futures-core/latest/futures_core/stream/trait.Stream.html)
for draining outgoing PDUs.

```toml
[dependencies]
hardy-btpu = { version = "0.1", features = ["tower"] }
```

### Receiver as a Service

```rust
use bytes::Bytes;
use hardy_btpu::{Receiver, ReceiverConfig, ReceiverEvent};
use tower::{ServiceBuilder, ServiceExt};

let mut rx = ServiceBuilder::new()
    .concurrency_limit(8)
    .service(Receiver::new(ReceiverConfig::default()));

let events = rx.ready().await?.call(pdu_bytes).await?;
for event in events {
    if let ReceiverEvent::BundleReceived(bundle) = event {
        // dispatch the reassembled bundle to the BPA
    }
}
```

### Sender as a Service (enqueue) + Stream (drain)

```rust
use bytes::Bytes;
use futures::StreamExt;
use hardy_btpu::{Sender, SenderConfig};
use tower::{Service, ServiceExt};

// With the `rand` feature, use `Sender::from_rng(..., &mut rng)` instead of
// passing an explicit `initial_transfer_number`.
let mut tx = Sender::new(SenderConfig::default(), initial_transfer_number);

// Enqueue a bundle. The response is `Some(transfer_number)` if segmented,
// or `None` if the bundle fit in a single Bundle message.
tx.ready().await?.call(Bytes::from(bundle_data)).await?;

// Drain outgoing PDUs as a stream.
while let Some(pdu) = tx.next().await {
    transmit_over_link(&pdu);
}
```

Both directions use Waker-based backpressure: `Service::poll_ready`
parks the caller while the transfer window is saturated (woken by
`Sender::complete`/`Sender::cancel`); `Stream::poll_next` parks while
the pending queue is empty (woken by the next `Service::call`). The
sender is treated as a perpetual source — `Stream::poll_next` never
yields `Ready(None)`.

`Sender` is **single-owner**: one task at a time mutates it via
`&mut self`. To fan-in enqueues from multiple producer tasks, wrap it
in `Arc<Mutex<_>>` or `tower::buffer::Buffer`.

## License

See the workspace-level license configuration.
