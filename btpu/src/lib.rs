#![no_std]
/*!
Bundle Transfer Protocol - Unidirectional (BTP-U) codec and transfer logic.

This crate implements the protocol defined in
[draft-ietf-dtn-btpu-02](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/)
for the unidirectional, unreliable transfer of binary objects (typically BPv7
bundles) over frame-based convergence layer protocols.

It also provides basic framework support for the FEC extension defined in
[draft-ietf-dtn-btpu-fec-01](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/).

# Overview

BTP-U sits between the Bundle Protocol and a convergence layer protocol, providing
segmentation, transfer windowing, interleaving, and repetition-based loss
protection without requiring IP services or a return channel.

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

# Architecture

The crate is a **pure protocol library** with no dependency on `hardy-bpa`,
`hardy-bpv7`, `std`, or any async runtime.  It is `#![no_std]` and uses only
`alloc`, suitable for embedded and CCSDS-frame deployments.  A
convergence-layer-specific CLA crate would use this library alongside
`hardy-bpa` to integrate with the BPA.

Three layers of abstraction are provided:

- **Low-level codec**: [`codec::decode_pdu`], [`codec::encode_message`],
  [`codec::pad_pdu`] for direct PDU manipulation.
- **Transfer management**: [`transfer::TransferWindow`],
  [`transfer::TransferNumberAllocator`] for building custom sender/receiver
  logic.
- **High-level sender/receiver**: [`sender::Sender`] and
  [`receiver::Receiver`] for typical CLA implementations.

# Features

- **`serde`**: Enables serialization support for configuration types.
- **`rand`**: Adds `from_rng` constructors for [`sender::Sender`] and
  [`transfer::TransferNumberAllocator`] that seed the initial transfer number
  from a `rand_core::Rng`. Without this feature, callers pass an explicit
  `initial_transfer_number: u32`.
- **`tower`**: Implements [`tower::Service<Bytes>`] for [`sender::Sender`]
  (enqueue) and [`receiver::Receiver`] (PDU dispatch), and
  [`futures_core::Stream`] (item: `BytesMut`) for [`sender::Sender`]
  (outgoing PDU drain). Enables composition with `tower::ServiceBuilder` for
  rate limiting, metrics, etc. Requires `std` at the consumer level (the
  `tower` crate itself is std-only).

  Both directions use waker-based backpressure: `Service::poll_ready` parks
  the caller while the transfer window is saturated (woken by
  `Sender::complete` / `Sender::cancel`); `Stream::poll_next` parks while the
  pending queue is empty (woken by the next `Service::call`). The sender is
  a perpetual source -- `poll_next` never yields `Ready(None)`. `Sender` is
  single-owner; to fan-in enqueues from multiple tasks, wrap it in
  `Arc<Mutex<_>>` or `tower::buffer::Buffer`.
*/

extern crate alloc;

pub mod codec;
pub mod fec;
pub mod header;
pub mod hint;
pub mod message;
pub mod receiver;
pub mod sender;
pub mod transfer;

#[cfg(feature = "tower")]
mod service;
