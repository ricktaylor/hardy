# Fuzz Test Plan: TCP Convergence Layer v4 (TCPCL)

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Network Transport Parsing & State Logic |
| **Module** | `hardy-tcpclv4` |
| **Target Directory** | `tcpclv4/fuzz/fuzz_targets/` |
| **Tooling** | `cargo fuzz` (libFuzzer) |
| **Test Suite ID** | FUZZ-TCPCL-01 |

## 1. Introduction

This document details the fuzz testing strategy for the `hardy-tcpclv4` module. As the primary network transport for the router, this module must be robust against malicious peers sending malformed frames, protocol violations, or infinite streams of garbage.

**Primary Objective:** Ensure the TCPCL session handler never panics, hangs, or allocates excessive memory when processing untrusted network input.

## 2. Fuzz Target Definitions

The strategy utilizes two complementary fuzz targets located in `tcpclv4/fuzz/fuzz_targets/`.

### 2.1 Target A: Protocol Stream (Parsing & State)

* **Source File:** `session_stream.rs`

* **Input:** Random byte stream (`Vec<u8>`).

* **Harness:**

  1. Create an in-memory `tokio::io::duplex` pipe.

  2. Spawn a `TcpclSession` attached to the server side of the pipe.

  3. Write the random input bytes to the client side.

* **Goal:** Verify that the **Parser** and **State Machine** handle:

  * Partial/Fragmented headers.

  * Invalid Magic Headers.

  * Unexpected message types (e.g., `XFER_ACK` before `SESS_INIT`).

  * Huge length fields (OOM protection).

### 2.2 Target B: Service Logic (Structured)

* **Source File:** `service_logic.rs`

* **Input:** `Arbitrary` generation of `TcpclMessage` structs.

* **Harness:**

  1. Instantiate the `TcpclService` directly (bypassing the parser).

  2. Feed it a sequence of valid message structs (e.g., `SessInit`, then `XferSegment`).

* **Goal:** Verify **Logic robustness**:

  * Does the session state machine handle valid messages in invalid orders?

  * Does receiving a `MSG_REJECT` cause a panic?

  * Does the logic handle `XFER_SEGMENT` flags correctly (Start/End/Middle)?

## 3. Vulnerability Classes & Mitigation

| Vulnerability Class | Description | Mitigation Strategy Verified |
 | ----- | ----- | ----- |
| **Parser OOM** | Header claims 4GB payload length. | Verify parser uses `bytes::Bytes` or streaming, not `Vec::with_capacity(len)`. |
| **State Confusion** | Sending data before handshake. | Verify session drops connection cleanly (or sends `MSG_REJECT`). |
| **Read Loop Hangs** | Input stream provides 1 byte at a time indefinitely. | Verify `async` read loop yields and checks timeouts. |
| **Fragmentation Panic** | `XFER_SEGMENT` logic fails on 0-length segments or overflowing offsets. | Verify checked math on offset calculation. |

## 4. Execution & Configuration

### 4.1 Running the Fuzzer

```bash
# Target A: Protocol Stream (Raw Bytes)
cargo fuzz run session_stream -- -max_total_time=1800 # 30 Mins

# Target B: Service Logic (Structs)
cargo fuzz run service_logic -- -max_total_time=1800
```

### 4.2 Sanitizer Configuration

AddressSanitizer (ASAN) is critical for detecting buffer over-reads during frame decoding.

```bash
export RUSTFLAGS="-Zsanitizer=address"
cargo fuzz run session_stream
```

## 5. Pass/Fail Criteria

* **PASS:** Zero crashes (panics) and zero timeouts (>10s hang per iteration).
* **FAIL:**
* **Panic:** `index out of bounds` in buffer parsing.
* **Timeout:** `await` on a read future that never completes (deadlock).

## 6. Corpus Management

* **Location:** `tcpclv4/fuzz/corpus/{target_name}/`
* **Seed Data:**
* **Stream:** Hex dump of a valid connection handshake (`dtn!04...`).
* **Service:** Serialized sequence of `SessInit`, `XferSegment` structs.
