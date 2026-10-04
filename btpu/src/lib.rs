#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
/*!
Bundle Transfer Protocol - Unidirectional (BTP-U) codec and transfer logic.

This crate implements the protocol defined in [draft-ietf-dtn-btpu] for the
unidirectional, unreliable transfer of binary objects (typically BPv7
bundles) over frame-based convergence layer protocols.

It also frames the messages of the FEC extension defined in
[draft-ietf-dtn-btpu-fec]; no FEC scheme is implemented.

[draft-ietf-dtn-btpu]: https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/
[draft-ietf-dtn-btpu-fec]: https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/

Section references throughout this crate's documentation and comments follow
draft-ietf-dtn-btpu-04 and draft-ietf-dtn-btpu-fec-02; this is the only place
in the code the target revisions are pinned.

# Overview

BTP-U sits between the Bundle Protocol and a convergence layer protocol,
providing segmentation and transfer windowing without requiring IP services
or a return channel.  The protocol additionally permits message interleaving
and repetition-based loss protection; this crate's sender interleaves only
by passing over a transfer that cannot supply its next segment, and does not
repeat messages (see [`sender::Sender`]).

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

Two layers of abstraction are provided:

- **Low-level codec**: [`codec::decode_pdu`], [`codec::encode_message`],
  [`codec::pad_pdu`] for direct PDU manipulation.  [`codec::decode_pdu_with`]
  takes [`codec::DecodeOptions`]: FEC decoding, and a
  [`codec::BundleExtent`] hook through which a caller that parses bundles
  lets the decoder delimit encapsulated bundles.
- **High-level sender/receiver**: [`sender::Sender`] and
  [`receiver::Receiver`] for typical CLA implementations, configured by
  [`sender::SenderConfig`] and [`receiver::ReceiverConfig`].  A receiver
  delivers each bundle whole, or streams it in order as it arrives.
  Receivers serving one link can share a `budget::RetentionBudget` (on
  targets with pointer-width atomics).

The [`transfer`] module holds the Section 5 types both layers above share:
[`transfer::WindowSize`] and the receiver's [`transfer::TransferId`].

# Features

- **`serde`**: `Serialize`/`Deserialize` for the configuration types
  ([`sender::SenderConfig`], [`receiver::ReceiverConfig`], and the validated
  newtypes they hold, which deserialize from plain integers with their range
  checks applied).
- **`rand`**: Adds `try_from_rng` and `from_rng` constructors for
  [`sender::Sender`] that seed the
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

  The sender's `Service::poll_ready` applies backpressure from the
  transfer window and the [`sender::SendQueueHighWatermark`]; the receiver's
  service is always ready and never fails.  How to run the `Stream` drain
  alongside producers, and how several producers may share one sender, is
  set out under Concurrency in [`sender::Sender`].
- **`critical-section`**: For targets without native atomic
  compare-and-swap (such as `thumbv6m`, Cortex-M0).  `bytes` reference-counts
  with atomics, and each sender and receiver takes its owner tag from a
  64-bit atomic counter; this feature switches both to `portable-atomic`
  with its critical-section fallback, which needs a `critical-section`
  implementation from the HAL or runtime.  The `budget` module, which
  shares an `Arc`, is compiled only on targets with pointer-width atomics.
*/

extern crate alloc;

use core::{error::Error, fmt, num::ParseIntError};

/// Implements the conversions every configuration newtype shares, as
/// `core::num::NonZero` does for its integer: `Display`, `Binary`, `Octal`,
/// `LowerHex`, and `UpperHex` forwarded to the field, `FromStr` returning
/// [`ParseError`], `TryFrom<$int>` returning [`OutOfRange`], and
/// `From<$ty> for $int`.  Naming a `NonZero` type as well adds `From` in
/// both directions, for newtypes that accept every non-zero value.
///
/// The type must have a `NAME` constant, `MIN` and `MAX` constants, and
/// `const fn new($int) -> Option<Self>` and `const fn get(self) -> $int`.
/// The [`OutOfRange`] carries no `max` when `MAX` is the integer's maximum.
/// Compile-time assertions check that `new` accepts exactly `MIN..=MAX`
/// at the boundaries, that `Option<$ty>` costs no space, and, for the
/// `NonZero` form, that `MIN..=MAX` is every non-zero value, since its
/// `From` bypasses `new`.
///
/// Defined above the `mod` declarations because `macro_rules!` scoping is
/// textual: only modules declared after it can use it.
macro_rules! config_newtype {
    ($ty:ident: $int:ty, $nz:ty) => {
        config_newtype!($ty: $int);

        // `From<$nz>` admits every non-zero value without calling `new`.
        const _: () = assert!($ty::MIN.get() == 1 && $ty::MAX.get() == <$int>::MAX);

        impl From<$nz> for $ty {
            fn from(n: $nz) -> Self {
                Self(n)
            }
        }

        impl From<$ty> for $nz {
            fn from(v: $ty) -> $nz {
                v.0
            }
        }
    };
    ($ty:ident: $int:ty) => {
        config_newtype!(@fmt $ty: Display, Binary, Octal, LowerHex, UpperHex);

        // `MIN` and `MAX` are what `OutOfRange` reports, so they must be
        // the bounds `new` applies.
        const _: () = assert!(
            $ty::new($ty::MIN.get()).is_some()
                && $ty::new($ty::MAX.get()).is_some()
                && ($ty::MIN.get() == 0 || $ty::new($ty::MIN.get() - 1).is_none())
                && ($ty::MAX.get() == <$int>::MAX || $ty::new($ty::MAX.get() + 1).is_none())
        );
        const _: () = assert!(size_of::<Option<$ty>>() == size_of::<$ty>());

        impl ::core::str::FromStr for $ty {
            type Err = $crate::ParseError;

            fn from_str(s: &str) -> ::core::result::Result<Self, $crate::ParseError> {
                let v = s.parse::<$int>().map_err(|source| $crate::ParseError::Syntax {
                    name: Self::NAME,
                    source,
                })?;
                Ok(Self::try_from(v)?)
            }
        }

        impl TryFrom<$int> for $ty {
            type Error = $crate::OutOfRange;

            // `as u64` widens: there is no `From<usize> for u64`, and every
            // integer used here fits.
            fn try_from(v: $int) -> ::core::result::Result<Self, $crate::OutOfRange> {
                Self::new(v).ok_or($crate::OutOfRange {
                    name: Self::NAME,
                    value: v as u64,
                    min: Self::MIN.get() as u64,
                    max: (Self::MAX.get() != <$int>::MAX).then_some(Self::MAX.get() as u64),
                })
            }
        }

        impl From<$ty> for $int {
            fn from(v: $ty) -> $int {
                v.get()
            }
        }
    };
    (@fmt $ty:ty: $($tr:ident),*) => {
        $(
            impl ::core::fmt::$tr for $ty {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    ::core::fmt::$tr::fmt(&self.0, f)
                }
            }
        )*
    };
}

#[cfg(target_has_atomic = "ptr")]
pub mod budget;
pub mod codec;
pub mod fec;
mod owner;
pub mod receiver;
pub mod sender;
pub mod transfer;

#[cfg(feature = "tower")]
mod service;

/// A configuration value outside the range its type accepts.
///
/// The error of every configuration newtype's `TryFrom`, and of its `FromStr`
/// through [`ParseError::OutOfRange`]: [`sender::PduSize`],
/// [`sender::SendQueueHighWatermark`], [`transfer::WindowSize`],
/// [`receiver::MaxTransferSize`], [`receiver::MaxSegments`], and
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

// Hand-written rather than derived with `thiserror`: the message takes a
// different form when there is no `max`, which one `#[error]` format string
// cannot express.
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
