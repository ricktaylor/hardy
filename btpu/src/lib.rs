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

- **Low-level codec**: [`decode_pdu`], [`encode_message`], [`pad_pdu`] for
  direct PDU manipulation.
- **Transfer management**: [`TransferWindow`], [`TransferNumberAllocator`]
  for building custom sender/receiver logic.
- **High-level sender/receiver**: [`Sender`] and [`Receiver`] for typical
  CLA implementations.

# Features

- **`serde`**: Enables serialization support for configuration types.
- **`rand`**: Adds `from_rng` constructors for [`Sender`] and
  [`TransferNumberAllocator`] that seed the initial transfer number from a
  `rand_core::RngCore`. Without this feature, callers pass an explicit
  `initial_transfer_number: u32`.
- **`tower`**: Implements [`tower::Service<Bytes>`] for [`Sender`] (enqueue)
  and [`Receiver`] (PDU dispatch), and [`futures_core::Stream`] (item:
  `BytesMut`) for [`Sender`] (outgoing PDU drain). Enables composition with
  `tower::ServiceBuilder` for rate limiting, metrics, etc. Requires `std`
  at the consumer level (the `tower` crate itself is std-only).
*/

extern crate alloc;

mod error;
pub mod fec;
mod header;
mod hint;
mod message;

mod codec;
mod receiver;
mod sender;
mod transfer;

#[cfg(feature = "tower")]
mod service;

// Re-exports: error
pub use error::Error;

// Re-exports: message types
pub use message::{
    FrameKind, Message, MessageFlags, MessageType, TransferEndMessage, TransferSegmentMessage,
    frame_kind,
};

// Re-exports: header
pub use header::{HEADER_SIZE, MAX_CONTENT_LENGTH};

// Re-exports: hints
pub use hint::{BUNDLE_LENGTH, HINT_HEADER_SIZE, HintItem};

// Re-exports: codec
pub use codec::{decode_pdu, encode_message, encoded_message_len, pad_pdu};

// Re-exports: transfer
pub use transfer::{
    DEFAULT_WINDOW_SIZE, MAX_WINDOW_SIZE, MIN_WINDOW_SIZE, TransferNumberAllocator,
    TransferValidity, TransferWindow,
};

// Re-exports: sender
pub use sender::{Sender, SenderConfig};

// Re-exports: receiver
pub use receiver::{Receiver, ReceiverConfig, ReceiverEvent};

/// A specialized `Result` type for BTP-U operations.
pub type Result<T> = core::result::Result<T, Error>;
