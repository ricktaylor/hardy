# hardy-btpu

A `no_std` implementation of the Bundle Transfer Protocol - Unidirectional (BTP-U): the wire codec, the transfer window, and a sender and receiver pair for one-way, frame-based links.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

This crate implements [draft-ietf-dtn-btpu](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/), which carries bundles over convergence layers that offer no IP services and no return channel (broadcast radio, satellite downlinks, CCSDS frames), and frames the messages of the FEC extension in [draft-ietf-dtn-btpu-fec](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/); no FEC scheme is implemented. It is a pure protocol library, `#![no_std]` with `alloc`, with no dependency on `hardy-bpa`, `hardy-bpv7`, or an async runtime, so a convergence-layer crate drives it against the link and composes it with `hardy-bpa`. There is no BTP-U convergence layer in Hardy yet; this crate is the protocol engine one would be built on.

Three layers are exposed, each usable without the ones above it: the `codec` module for PDU encode and decode, the `transfer` module for the window and transfer-number primitives, and the `sender` and `receiver` modules for a ready-to-use engine configured by `SenderConfig` and `ReceiverConfig`.

## Features

- **Segmentation and reassembly**: bundles that fit a PDU travel as a single message; larger ones are cut into segments against the configured PDU size and reassembled at the receiver, with out-of-order and repeated messages handled.
- **Transfer window**: the Section 5 sliding window, enforced on the span of outstanding transfer numbers at the sender and with roll-over at the receiver; the sender releases a transfer's slot itself when its last segment is packed.
- **Fault containment**: receiving is infallible, every fault and policy disposition is a `ReceiverEvent`, and a malformed message is skipped via its header length without discarding the rest of the PDU.
- **Bounded memory**: a mandatory per-transfer cap on reassembled bundle size, with per-segment and hint bookkeeping budgeted separately so a flood of tiny segments cannot exhaust the receiver, and an optional per-transfer segment limit, derivable from the link's PDU size, for links with small frames.
- **Link framing**: fixed-size padded PDUs (CCSDS-style) or variable-length PDUs (datagrams), optionally emitting fitting bundles as bare frames on links shared with raw-bundle peers, through the same queue as everything else.
- **Forward compatibility**: unknown message types, unknown hints, and reserved flag bits relay byte-exact; the provisional FEC message types decode only when switched on.
- Feature flag: `serde` -- `Serialize`/`Deserialize` for `SenderConfig`, `ReceiverConfig`, and the validated newtypes they hold, which re-validate on deserialize.
- Feature flag: `rand` -- `try_from_rng` and `from_rng` constructors that seed the initial transfer number from an RNG, such as the operating system's `rand::rngs::SysRng`.
- Feature flag: `tower` -- `tower::Service` implementations for `Sender` and `Receiver` and a `futures_core::Stream` PDU drain, with waker-based backpressure. Requires `std` at the consumer level.
- Feature flag: `critical-section` -- builds on targets without native atomic compare-and-swap (such as `thumbv6m`, Cortex-M0) by switching `bytes` to `portable-atomic`'s critical-section fallback. Requires a `critical-section` implementation from the HAL or runtime.

## Usage

A loopback, with the `rand` feature enabled: bundles enqueued on a `Sender` are drained as PDUs, fed to a `Receiver`, and come back out as events.

```rust
use bytes::Bytes;
use hardy_btpu::receiver::{Receiver, ReceiverConfig, ReceiverEvent};
use hardy_btpu::sender::{SendOptions, Sender, SenderConfig};
use rand::rngs::SysRng;

// The configuration types validate their spec-defined ranges on
// construction; the defaults (1500-byte PDUs, a window of 16, fixed-size
// framing, a 1 GiB bundle-size cap) are always valid.  The spec recommends
// an unpredictable initial transfer number, so the sender draws it from the
// operating system RNG (the `rand` feature); without that feature, pass one
// to `Sender::new`.
let mut sender = Sender::try_from_rng(SenderConfig::default(), &mut SysRng)?;
let mut receiver = Receiver::new(ReceiverConfig::default());

let bundle = Bytes::from(vec![0x9F; 4000]); // stands in for an encoded bundle
let id = sender.enqueue(bundle.clone(), SendOptions::default())?;

// Drain PDUs to the link; here the link is the receiver.  A segmented
// transfer's window slot is released when its last PDU is packed, so the
// caller has nothing to acknowledge.  Each PDU lists the bundles it
// carries; the one flagged `completes` holds the bundle's last bytes.
let mut delivered = Vec::new();
let mut sent = false;
while let Some(pdu) = sender.next_pdu() {
    for event in receiver.receive_pdu(pdu.data) {
        match event {
            ReceiverEvent::BundleReceived { data, .. } => delivered.push(data),
            _ => {} // cancellations, expiries, drops, faults: see ReceiverEvent
        }
    }
    sent |= pdu.bundles.iter().any(|c| c.id == id && c.completes);
}
assert!(sent);
assert_eq!(delivered, vec![bundle]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Wire-format details, event semantics, memory bounds, and the padding pitfalls of bare bundle frames are documented in the rustdoc.

## Documentation

- [Design](docs/design.md)
- [Changelog](CHANGELOG.md)
- [API Documentation](https://docs.rs/hardy-btpu)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
