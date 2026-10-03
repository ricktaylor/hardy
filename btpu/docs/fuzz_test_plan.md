# Fuzz Test Plan: BTP-U

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Convergence Layer Protocol Engine (Robustness) |
| **Module** | `hardy-btpu` |
| **Target Source** | `btpu/fuzz/fuzz_targets/decode.rs`, `btpu/fuzz/fuzz_targets/receive.rs` |
| **Tooling** | `cargo-fuzz` (libFuzzer) + `sanitizers` |
| **Test Suite ID** | FUZZ-BTPU-01 |
| **Version** | 1.0 |

## 1. Introduction

This document details the fuzz testing strategy for the `hardy-btpu` module. A BTP-U receiver takes link-layer PDUs from an unauthenticated, unidirectional link with no way to push back on the sender, so every byte it decodes, and every sequence of PDUs it accumulates state from, is attacker-controlled.

**Primary Objective:** Verify that the codec and the receiver handle arbitrary input without panicking, overrunning a buffer, or looping, that the codec's encode and decode paths agree on everything the decoder yields, and that the receiver's event and size guarantees hold for any PDU sequence.

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

For every PDU:

1. **Delivered bundles:** every `Received` bundle is non-empty and no larger than the cap.

2. **PDU faults:** a `MalformedPdu` event is the last event of its PDU.

**Coverage Scope:** reassembly, repeats, and sequence checks (Sections 4, 6, 8.1 to 8.3), Transfer Cancel (Section 8.4), the window across the roll-over (Section 5), the FEC transfer rules (`draft-ietf-dtn-btpu-fec-02` Section 3), and the per-transfer and retention limits (Section 10).

## 4. Vulnerability Classes & Mitigation

| Vulnerability Class | Description | Mitigation Strategy Verified |
 | ----- | ----- | ----- |
| **Length Field Overrun** | A message length runs past the PDU, or a hint chain past its message. | Bounds-checked slicing; the fault is terminal for the PDU and earlier messages are kept. |
| **32-bit Truncation** | A wire-derived length or segment index is cast to `usize`. | Lengths compared before conversion; the `decode` round trip catches a truncated re-encode. |
| **Encode/Decode Disagreement** | The encoder writes a different length or different bytes than the decoder read. | Invariants 1 to 3 of the `decode` target. |
| **Unbounded Memory Growth** | Tiny segments, repeated hints, or many concurrent transfers hold more state than configured. | The transfer cap, bookkeeping budget, segment limit, and retention limit, reached with the configurations above. |
| **Window Arithmetic** | Transfer numbers near the 2³² roll-over misclassify or never expire. | Accumulated state across arbitrary transfer numbers. |
| **Hook Misbehaviour** | The bundle-extent hook claims zero bytes or more than remains. | The decoder treats the claim as terminal rather than slicing past the PDU. |

## 5. Execution & Configuration

### 5.1 Running the Fuzzer

From the `btpu/` directory (the fuzz crate pins a nightly toolchain):

```bash
# Run for a set duration (Regression Mode)
cargo fuzz run decode -- -max_total_time=1800 # 30 Mins
cargo fuzz run receive -- -max_total_time=1800

# Reach the oversized-bundle branch of the decode target
cargo fuzz run decode -- -max_len=1100000

# Run indefinitely (Discovery Mode)
cargo fuzz run receive -j $(nproc)
```

Both targets are built and run in CI by ClusterFuzzLite, which discovers every `*/fuzz` crate and its `[[bin]]` targets (see [`.clusterfuzzlite/build.sh`](../../.clusterfuzzlite/build.sh)).

### 5.2 Sanitizer Configuration

`cargo-fuzz` builds with AddressSanitizer by default, which catches out-of-bounds reads in the slicing paths.

## 6. Pass/Fail Criteria

* **PASS:** Each target runs for the defined duration with **zero** crashes.
* **FAIL:** The fuzzer creates a `crash-*`, `timeout-*`, or `oom-*` artifact.
  * **Action:** Reproduce with `cargo fuzz run <target> <artifact>` and inspect with `cargo fuzz fmt <target> <artifact>`.
  * **Remediation:** Fix the defect and add the input as a regression test in `tests/codec.rs` or `tests/receiver.rs`.

## 7. Corpus Management

No seed corpus is checked in; libFuzzer starts from an empty input. Seeds would shorten discovery, particularly for `receive`, whose interesting states need several well-formed PDUs in sequence.

* **Location:** `btpu/fuzz/corpus/decode/`, `btpu/fuzz/corpus/receive/`
* **Suggested Seed Data:**
  * PDUs produced by `Sender::next_pdu` for small, segmented, and hinted bundles.
  * A Transfer Cancel after a partial transfer.
  * Bare BPv6 (`0x06`) and BPv7 (`0x9F`) frames.
  * FEC messages with both pre-agreed and explicit FEC Instance IDs.
