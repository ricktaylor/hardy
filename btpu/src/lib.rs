#![no_std]
/*!
Bundle Transfer Protocol - Unidirectional (BTP-U) codec and transfer logic.

This crate implements the protocol defined in
[draft-ietf-dtn-btpu](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/)
for the unidirectional, unreliable transfer of binary objects (typically BPv7
bundles) over frame-based convergence layer protocols.

It also frames the messages of the FEC extension defined in
[draft-ietf-dtn-btpu-fec](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/);
no FEC scheme is implemented.

Section references throughout this crate's documentation and comments follow
draft-ietf-dtn-btpu-04 and draft-ietf-dtn-btpu-fec-02; this is the only place
the target revisions are pinned.

# Overview

BTP-U sits between the Bundle Protocol and a convergence layer protocol,
providing segmentation and transfer windowing without requiring IP services
or a return channel.  The protocol additionally permits message interleaving
and repetition-based loss protection; this crate's sender implements neither
yet (see [`sender::Sender`]).

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
  [`codec::pad_pdu`] for direct PDU manipulation.  [`codec::decode_pdu_with`]
  takes [`codec::DecodeOptions`]: FEC decoding, and a
  [`codec::BundleExtent`] hook through which a caller that parses bundles
  lets the decoder delimit encapsulated bundles.
- **Transfer management**: [`transfer::TransferWindow`],
  [`transfer::TransferNumberAllocator`] for building custom sender/receiver
  logic.
- **High-level sender/receiver**: [`sender::Sender`] and
  [`receiver::Receiver`] for typical CLA implementations, configured by
  [`sender::SenderConfig`] and [`receiver::ReceiverConfig`].

# Features

- **`serde`**: `Serialize`/`Deserialize` for the configuration types
  ([`sender::SenderConfig`], [`receiver::ReceiverConfig`], and the validated
  newtypes they hold, which deserialize from plain integers with their range
  checks applied).
- **`rand`**: Adds `try_from_rng` and `from_rng` constructors for
  [`sender::Sender`] and [`transfer::TransferNumberAllocator`] that seed the
  initial transfer number from a `rand_core::TryRng` (such as the operating
  system's `rand::rngs::SysRng`) or `rand_core::Rng`. Without this feature,
  callers pass an explicit `initial_transfer_number: u32`.
- **`tower`**: Implements `tower::Service<SendRequest>` for
  [`sender::Sender`] (enqueue; `SendRequest: From<Bytes>`) and
  `tower::Service<Bytes>` for [`receiver::Receiver`] (PDU dispatch), and
  `futures_core::Stream` (item: [`sender::Pdu`]) for [`sender::Sender`]
  (outgoing PDU drain). Enables composition with `tower::ServiceBuilder` for rate
  limiting, metrics, etc. Requires `std` at the consumer level (the `tower`
  crate itself is std-only).

  Both directions use waker-based backpressure: `Service::poll_ready` parks
  the caller while the transfer window is saturated or the send queue is at
  its configured [`sender::SendQueueDepth`], and wakes every parked task
  when `Stream::poll_next` drains a PDU (which also releases the window
  slot of any Transfer End it packed) or `Sender::cancel` frees a slot or
  a queue entry;
  `Stream::poll_next` parks while the pending queue is empty (woken by the
  next `Service::call`). The sender is a perpetual source: `poll_next` never
  yields `Ready(None)`. `Sender` is single-owner; to fan in enqueues from
  multiple tasks, wrap it in `Arc<Mutex<_>>` (not `tower::buffer::Buffer`,
  which strands the `Stream` half and `cancel` in its worker task) and do
  `poll_ready` and `call` under one lock hold, since admission is not
  reserved between them.
- **`critical-section`**: For targets without native atomic
  compare-and-swap (such as `thumbv6m`, Cortex-M0).  The crate itself uses
  no atomics, but `bytes` reference-counts with them; this feature switches
  `bytes` to `portable-atomic` with its critical-section fallback, which
  needs a `critical-section` implementation from the HAL or runtime.
*/

extern crate alloc;

use core::{error::Error, fmt, num::ParseIntError};

/// Implements `Display`, `Binary`, `Octal`, `LowerHex`, and `UpperHex` for a
/// newtype by forwarding to its single field, as `core::num::NonZero` does.
macro_rules! forward_integer_fmt {
    ($ty:ty) => {
        forward_integer_fmt!(@impl $ty: Display, Binary, Octal, LowerHex, UpperHex);
    };
    (@impl $ty:ty: $($tr:ident),*) => {
        $(
            impl ::core::fmt::$tr for $ty {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    ::core::fmt::$tr::fmt(&self.0, f)
                }
            }
        )*
    };
}

pub mod codec;
pub mod fec;
pub mod receiver;
pub mod sender;
pub mod transfer;

#[cfg(feature = "tower")]
mod service;

/// A configuration value outside the range its type accepts.
///
/// The error of every configuration newtype's `TryFrom`, and of its `FromStr`
/// through [`ParseError::OutOfRange`]: [`sender::PduSize`],
/// [`sender::SendQueueDepth`], [`transfer::WindowSize`],
/// [`receiver::MaxBundleSize`], [`receiver::MaxSegments`], and
/// [`receiver::MaxRetainedBytes`].  Values are
/// widened to `u64` so one type covers all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfRange {
    /// What the value configures, in the words the message uses.
    pub name: &'static str,
    /// The rejected value.
    pub value: u64,
    /// The least valid value.
    pub min: u64,
    /// The greatest valid value, or `None` when every value from `min` up
    /// to the integer type's maximum is valid.
    pub max: Option<u64>,
}

impl fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            name,
            value,
            min,
            max,
        } = self;
        match max {
            Some(max) => write!(f, "Invalid {name} {value} (must be {min}..={max})"),
            None => write!(f, "Invalid {name} {value} (must be at least {min})"),
        }
    }
}

impl Error for OutOfRange {}

/// A configuration value that could not be parsed from a string.
///
/// The error of every configuration newtype's `FromStr`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// The string is not an unsigned integer of the newtype's width.
    #[error("Invalid {name}: {source}")]
    Syntax {
        /// What the value configures, in the words the message uses.
        name: &'static str,
        /// Why the integer did not parse.
        source: ParseIntError,
    },
    /// The integer parsed but is outside the newtype's range.
    #[error(transparent)]
    OutOfRange(#[from] OutOfRange),
}

/// Compiles the README's code blocks as doctests so the example there
/// cannot drift from the API.  The example seeds its sender from the
/// operating system RNG, so it needs the `rand` feature.
#[cfg(all(doctest, feature = "rand"))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;
