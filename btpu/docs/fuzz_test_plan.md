# Fuzz Test Plan: BTP-U

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Convergence Layer Protocol Engine (Robustness) |
| **Module** | `hardy-btpu` |
| **Target Source** | `btpu/fuzz/fuzz_targets/decode.rs`, `btpu/fuzz/fuzz_targets/receive.rs`, `btpu/fuzz/fuzz_targets/send.rs` |
| **Tooling** | `cargo-fuzz` (libFuzzer) + `sanitizers` |
| **Test Suite ID** | FUZZ-BTPU-01 |
| **Version** | 1.2 |

## 1. Introduction

This document details the fuzz testing strategy for the `hardy-btpu` module. A BTP-U receiver takes link-layer PDUs from an unauthenticated, unidirectional link with no way to push back on the sender, so every byte it decodes, and every sequence of PDUs it accumulates state from, is attacker-controlled. The sender takes no input from the link, but a CLA drives it through arbitrary interleavings of begin, push, finish, cancel, and drain, and its packing has more state than unit tests enumerate.

**Primary Objective:** Verify that the codec and the receiver handle arbitrary input without panicking, overrunning a buffer, or looping, that the codec's encode and decode paths agree on everything the decoder yields, that the receiver's event and size guarantees hold for any PDU sequence, and that the sender's call, PDU, and delivery guarantees hold for any sequence of calls.

## 2. Requirements Mapping

No Low-Level Requirements are assigned to this crate (see [`UTP-BTPU-01`](unit_test_plan.md) Section 2). The plan verifies:

| LLR ID | Description |
 | ----- | ----- |
| [**REQ-14**](../../docs/requirements.md#req-14-reliability) | Fuzz testing of all external APIs. |

## 3. Fuzz Target Definition

### 3.1 Target: `decode`

The harness decodes the input as one PDU three times: with default options, with FEC decoding on, and with FEC decoding on plus a bundle-extent hook that takes every `0x9F` bundle to be eight bytes long. For every message the decoder yields:

1. **Re-encoding:** `encode_message` succeeds and writes exactly `encoded_message_len` bytes.

2. **Byte-exact relay:** an `Unknown` message re-encodes to the bytes it was decoded from.

3. **Round trip:** decoding the re-encoded bytes yields the same message and nothing else.

4. **Oversized bundles:** a `Bundle` longer than `MAX_CONTENT_LENGTH` (an encapsulated bundle delimited by the hook or a bare frame) refuses to encode with `LengthOverflow` and leaves the buffer empty. Reaching this branch needs an input over 1 MiB, so it runs only with `-max_len` raised above libFuzzer's default of 4096.

After iteration the decoder **MUST** report itself exhausted.

**Coverage Scope:** message header and length field (Section 7), flags (Section 7.1), hint chains and the Bundle Length hint (Sections 7.2, 9.1), unknown types and hints (Section 7.3), every message definition (Section 8) and the FEC message definitions (`draft-ietf-dtn-btpu-fec-02` Section 4), padding (Sections 8.5, 8.6), and bare and encapsulated bundle frames (Section 12.1).

### 3.2 Target: `receive`

The input is a flags byte followed by a sequence of PDUs, each prefixed by a one-byte length, fed to one receiver so that window, reassembly, and cancellation state accumulate across PDUs. The receiver has a window of 4 and a transfer cap of 128 bytes, below the 255-byte longest PDU, so the oversize gates are reachable. The flags byte selects:

| Bit | Configuration |
 | ----- | ----- |
| 0 | FEC decoding |
| 1 | A segment limit derived from a 64-byte link PDU, so the limit rather than the per-segment charge bounds segment count |
| 2 | The bundle-extent hook from the `decode` target |
| 3 | A retention limit equal to the cap, below one transfer's allowance, so a lone transfer can be refused as `ReceiverFull` |
| 4 | Streamed delivery |
| 5 | A shared retention budget of half the cap, so a transfer can be refused as `BudgetFull` |
| 6 | Refusal: once a streamed transfer has released more than 32 bytes, the harness refuses it, and a second refusal of the same id is ignored |

For every PDU:

1. **Delivered bundles:** every `Received` bundle is non-empty and no larger than the cap.

2. **PDU faults:** a `MalformedPdu` event is the last event of its PDU.

3. **Streamed transfers:** each transfer id starts at most once; every `TransferData` is non-empty and follows its `TransferStarted`; the bytes a transfer releases, its `TransferFinished` data included, total no more than the cap; a finished transfer released at least one byte.

4. **Dropped messages:** a `MessageDropped` carries an id only when its reason is neither `OutsideWindow` nor `UnknownTransfer`, and that id names the dropped transfer number.

5. **Budget accounting:** with a shared budget, what the budget has charged equals what the receiver reports retained, and never exceeds the limit.

**Coverage Scope:** reassembly, repeats, and sequence checks (Sections 4, 6, 8.1 to 8.3), Transfer Cancel (Section 8.4), the window across the roll-over (Section 5), the FEC transfer rules (`draft-ietf-dtn-btpu-fec-02` Section 3), the per-transfer, retention, and shared budget limits (Section 10), and streamed release of the contiguous prefix.

### 3.3 Target: `send`

The input is two configuration bytes followed by a sequence of operations on one sender, whose PDUs feed a receiver over a lossless link. An operation byte selects one of: `begin` or `enqueue` of a bundle of 5 to 1500 bytes; a push of up to 255 or up to 1020 bytes into an open bundle, so overruns are common; `finish`; a cancel of an open handle or of an enqueued bundle; and `next_pdu`, with or without a flush. The top bit of the operation byte attaches an unknown hint to a new bundle. Every bundle starts with `0x9F` and carries a serial number, so each is distinct and a bare frame may carry it. The first configuration byte selects:

| Bit | Configuration |
 | ----- | ----- |
| 0, 1 | Fixed-size framing, variable framing, variable framing with a 46-byte floor, or variable framing with bare bundle frames |
| 2 | A send queue high watermark of 16 bytes, below one segment, so pushes past the watermark are common |
| 3 | `SegmentCutStrategy::Half` |
| 4 | An initial transfer number of `u32::MAX - 1`, so transfer numbers roll over |

The second sets the PDU size, 24 to 279 bytes. The sender and receiver windows are both 4.

1. **Calls:** admission is refused only for a full window, a PDU too small to carry a segment, or a bundle needing too many segments; a push past the bundle's length fails with `Overrun`; `finish` succeeds exactly when every byte was pushed and otherwise fails with `Underrun`; cancelling an enqueued bundle that has not completed succeeds.

2. **PDUs:** every PDU is no longer than the PDU size and no shorter than the floor, and exactly the PDU size under fixed-size framing.

3. **Carried lists:** a PDU names each bundle at most once, names only bundles that are live, and marks every Bundle Message and bare frame as completing.

4. **Push readiness:** whenever an open handle is not push-ready, `next_pdu` returns a PDU.

5. **Delivery:** the receiver reports only `Received` and `TransferCancelled`; no bundle arrives twice and no cancelled bundle arrives.

At the end the harness pushes and finishes every open bundle and drains the sender. The sender then has nothing pending, no queued bytes, and a free window slot, and the receiver has delivered exactly the bundles that were neither cancelled nor finished short.

**Coverage Scope:** segmentation (Section 4), Transfer Cancel (Section 8.4), the window across the roll-over (Section 5), hints on the first segment (Section 7.2), padding and the floor (Sections 8.5, 8.6), bare frames (Section 12.1), and the sender's own policy: pack-time cuts under both strategies, passing over a transfer that cannot supply, the queue bound, and flush.

## 4. Vulnerability Classes & Mitigation

| Vulnerability Class | Description | Mitigation Strategy Verified |
 | ----- | ----- | ----- |
| **Length Field Overrun** | A message length runs past the PDU, or a hint chain past its message. | Bounds-checked slicing; the fault is terminal for the PDU and earlier messages are kept. |
| **32-bit Truncation** | A wire-derived length or segment index is cast to `usize`. | Lengths compared before conversion; the `decode` round trip catches a truncated re-encode. |
| **Encode/Decode Disagreement** | The encoder writes a different length or different bytes than the decoder read. | Invariants 1 to 3 of the `decode` target. |
| **Unbounded Memory Growth** | Tiny segments, repeated hints, or many concurrent transfers hold more state than configured. | The transfer cap, bookkeeping budget, segment limit, retention limit, and shared budget, reached with the configurations above. |
| **Window Arithmetic** | Transfer numbers near the 2³² roll-over misclassify or never expire. | Accumulated state across arbitrary transfer numbers. |
| **Scheduling State** | A cancel, overrun, or flush leaves a transfer half-sent, holds a window slot, or leaks queued bytes. | The end state and delivery checks of the `send` target. |
| **Hook Misbehaviour** | The bundle-extent hook claims zero bytes or more than remains. | The decoder treats the claim as terminal rather than slicing past the PDU. |

## 5. Execution & Configuration

### 5.1 Running the Fuzzer

From the `btpu/` directory (the fuzz crate pins a nightly toolchain):

```bash
# Run for a set duration (Regression Mode)
cargo fuzz run decode -- -max_total_time=1800 # 30 Mins
cargo fuzz run receive -- -max_total_time=1800
cargo fuzz run send -- -max_total_time=1800

# Reach the oversized-bundle branch of the decode target
cargo fuzz run decode -- -max_len=1100000

# Run indefinitely (Discovery Mode)
cargo fuzz run receive -j $(nproc)
```

All three targets are built and run in CI by ClusterFuzzLite, which discovers every `*/fuzz` crate and its `[[bin]]` targets (see [`.clusterfuzzlite/build.sh`](../../.clusterfuzzlite/build.sh)).

### 5.2 Sanitizer Configuration

`cargo-fuzz` builds with AddressSanitizer by default, which catches out-of-bounds reads in the slicing paths.

## 6. Pass/Fail Criteria

* **PASS:** Each target runs for the defined duration with **zero** crashes.
* **FAIL:** The fuzzer creates a `crash-*`, `timeout-*`, or `oom-*` artifact.
  * **Action:** Reproduce with `cargo fuzz run <target> <artifact>` and inspect with `cargo fuzz fmt <target> <artifact>`.
  * **Remediation:** Fix the defect and add the input as a regression test in `tests/codec.rs`, `tests/receiver.rs`, `tests/sender.rs`, or `tests/send_handle.rs`.

## 7. Corpus Management

No seed corpus is checked in; libFuzzer starts from an empty input. Seeds would shorten discovery, particularly for `receive`, whose interesting states need several well-formed PDUs in sequence.

* **Location:** `btpu/fuzz/corpus/decode/`, `btpu/fuzz/corpus/receive/`, `btpu/fuzz/corpus/send/`
* **Suggested Seed Data:**
  * PDUs produced by `Sender::next_pdu` for small, segmented, and hinted bundles.
  * A Transfer Cancel after a partial transfer.
  * Bare BPv6 (`0x06`) and BPv7 (`0x9F`) frames.
  * FEC messages with both pre-agreed and explicit FEC Instance IDs.
