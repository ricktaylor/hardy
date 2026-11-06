# hardy-frame

Stackable transmit and receive halves for the frame services below Hardy's convergence layers.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

Several of Hardy's convergence layers have the same shape: a protocol engine (BTP-U, LTP) turns bundles into frames and frames back into bundles, and below it sits a stack of frame services, each carrying bounded, whole-or-nothing units: AOS transfer frames, Encapsulation or Space Packets, Ethernet frames, UDP datagrams, QUIC DATAGRAM frames. This crate is the one interface every layer of such a stack offers above and takes from below, so that an engine runs over any stack whose frames it can use, and a stack is a type assembled where it is deployed: `Ltp<Epp<Aos<Modem>>>` for a CCSDS link, `Ltp<Udp>` against another implementation.

The crate holds the traits and nothing else. Layers, links and the channel-backed halves most layers build on are separate crates, and so are the engines; `hardy-btpu` takes its link framing from a `FrameSize` behind its `frame` feature.

## Features

- A transmit half, `FrameTx`, with two-phase sending: `poll_reserve` (or `reserve`, the same wait as a future) waits for room for one frame and lends the memory the frame is written into, and the `Permit` it returns is sent with the frame's length and never waits. A packer such as BTP-U packs only once it holds a permit, so a packed frame always has somewhere to go.
- Headers written in place: a layer lends the slice behind its header and writes the header in front when the frame is sent, so a frame reaches the link where the engine wrote it, which can be a ring slot or a DMA buffer.
- A receive half, `FrameRx`, that is a pull `Stream` of frames, with backpressure by not polling and an end that means the service has closed.
- A `FrameSize` per half, fixed for its life, from which a layer derives its own size and a stack is checked when it is assembled.
- `no_std`, runtime-agnostic and statically dispatched: the crate depends only on `core`, `alloc`, `bytes`, `futures-core` and `thiserror`, and no future is boxed.
- Feature flag: `critical-section` -- builds on targets without native atomic compare-and-swap (such as `thumbv6m`, Cortex-M0) by switching `bytes` to `portable-atomic`'s critical-section fallback. Requires a `critical-section` implementation from the HAL or runtime.

## Usage

A layer that adds a two-byte length in front of every frame, over any transmit half:

```rust
use core::task::{Context, Poll};

use hardy_frame::{FrameSize, FrameTx, Permit, SendError};

const HEADER: usize = 2;

struct Header<T> {
    below: T,
    size: FrameSize,
}

impl<T: FrameTx> Header<T> {
    /// `None` if the half below has no room behind the header.
    fn new(below: T) -> Option<Self> {
        let size = below.frame_size().behind(HEADER)?;
        Some(Self { below, size })
    }
}

struct HeaderPermit<P>(P);

impl<P: Permit> AsMut<[u8]> for HeaderPermit<P> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.0.as_mut()[HEADER..]
    }
}

impl<P: Permit> Permit for HeaderPermit<P> {
    fn send(mut self, len: usize) -> Result<(), SendError> {
        let header = u16::try_from(len).expect("a frame fits the length field");
        self.0.as_mut()[..HEADER].copy_from_slice(&header.to_be_bytes());
        self.0.send(len + HEADER)
    }
}

impl<T: FrameTx> FrameTx for Header<T> {
    type Permit<'a>
        = HeaderPermit<T::Permit<'a>>
    where
        Self: 'a;

    fn frame_size(&self) -> FrameSize {
        self.size
    }

    fn poll_reserve(&self, cx: &mut Context<'_>) -> Poll<Self::Permit<'_>> {
        self.below.poll_reserve(cx).map(HeaderPermit)
    }
}
```

## Documentation

- [Design](docs/design.md)
- [API Documentation](https://docs.rs/hardy-frame)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
