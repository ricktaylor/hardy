# hardy-btpu

A pure Rust implementation of the **Bundle Transfer Protocol - Unidirectional
(BTP-U)** as defined in
[draft-ietf-dtn-btpu](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu/),
with framework support for the FEC extension defined in
[draft-ietf-dtn-btpu-fec](https://datatracker.ietf.org/doc/draft-ietf-dtn-btpu-fec/).

BTP-U provides unidirectional, unreliable transfer of binary objects
(typically BPv7 bundles) over frame-based convergence layer protocols --
segmentation, transfer windowing, message interleaving, repetition-based
loss protection, and pluggable Forward Error Correction -- without requiring
IP services or a return channel. This makes it suitable for one-way links
such as broadcast radio, satellite, and CCSDS frames.

The crate is a pure protocol library: `#![no_std]` with `alloc`, no
dependency on `hardy-bpa`, `hardy-bpv7`, or any async runtime, and zero-copy
buffers via `bytes`. A convergence-layer-specific CLA crate would use it
alongside `hardy-bpa` to integrate with the Bundle Protocol Agent.

Three layers of abstraction:

- **`codec`** -- low-level PDU encode/decode, including transparent
  handling of bare BPv6/BPv7 bundle frames on shared links.
- **`transfer`** -- window and transfer-number primitives for building
  custom sender/receiver logic.
- **`sender` / `receiver`** -- ready-to-use `Sender` and `Receiver` for
  typical CLA implementations.

Wire-format details, event semantics, and the FEC framework are documented
in the rustdoc (`cargo doc --open`).

## Example

```rust
use bytes::Bytes;
use hardy_btpu::receiver::{Receiver, ReceiverConfig, ReceiverEvent};
use hardy_btpu::sender::{Sender, SenderConfig};

// The spec recommends an unpredictable initial transfer number; with the
// `rand` feature, use `Sender::from_rng(config, &mut rng)` instead.
let mut sender = Sender::new(SenderConfig::default(), read_hw_rng());
sender.enqueue(Bytes::from(bundle_data))?;
while sender.has_pending() {
    transmit_over_link(&sender.next_pdu().unwrap());
}

// Receiver side
let mut receiver = Receiver::new(ReceiverConfig::default());
for pdu in received_pdus {
    for event in receiver.receive_pdu(pdu)? {
        match event {
            ReceiverEvent::BundleReceived(bundle) => {
                // Pass the reassembled bundle to the BPA
            }
            _ => {} // cancellations, expiries, drops -- see ReceiverEvent
        }
    }
}
```

## Features

All default-off:

- **`serde`** -- `Serialize`/`Deserialize` for the configuration types.
- **`rand`** -- `from_rng` constructors that seed the initial transfer
  number from a `rand_core::Rng`.
- **`tower`** -- `tower::Service` implementations for `Sender` and
  `Receiver`, plus a `futures_core::Stream` PDU drain with waker-based
  backpressure. Requires `std` at the consumer level.

## License

See the workspace-level license configuration.
