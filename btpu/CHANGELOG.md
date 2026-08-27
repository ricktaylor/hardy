# Changelog

All notable changes to `hardy-btpu` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial release: a `#![no_std]` + `alloc` implementation of the Bundle Transfer Protocol - Unidirectional (draft-ietf-dtn-btpu) with framework types for the FEC extension (draft-ietf-dtn-btpu-fec). The crate is a pure protocol library with no dependency on `hardy-bpa`, `hardy-bpv7`, or an async runtime; a convergence-layer crate composes it with `hardy-bpa`.
- `codec`: the wire format. `decode_pdu` is a lazy, zero-copy iterator with two-tier fault containment: a malformed message interior is skipped via its header length and iteration continues, while a framing fault stops the walk and keeps everything already parsed. Unknown message types and their flag bits relay byte-exact; bare BPv6/BPv7 frames on shared links decode as a Bundle message.
- `transfer`: the wraparound-safe receive window and the sender's transfer-number allocator, which enforces the Section 5 rule by gating on the span of outstanding numbers rather than their count.
- `sender`/`receiver`: segmentation, PDU packing, and window-bounded cancellation on the send side; reassembly, window expiry, and duplicate/conflict rejection on the receive side. `Receiver::receive_pdu` is infallible: every fault and disposition is a `ReceiverEvent`, so a fault late in a PDU never discards the events before it.
- Validated configuration newtypes (`PduSize`, `WindowSize`, `MaxBundleSize`, `SendQueueDepth`) so invalid sizes are `TryFrom` errors at the edge and no constructor panics. Memory is bounded by default: the bundle-size cap is mandatory and enforced during accumulation, and the pending send queue is depth-bounded.
- Optional features: `serde` (the newtypes serialize as plain integers with validation on deserialize), `rand` (`from_rng` constructors seeding the initial transfer number), and `tower` (`Service` impls for `Sender` and `Receiver` plus a `Stream` PDU drain with waker-based backpressure on both window span and send-queue depth).
