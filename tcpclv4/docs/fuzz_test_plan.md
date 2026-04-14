# Fuzz Test Plan: TCP Convergence Layer v4 (TCPCL)

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Network Transport Parsing & State Logic |
| **Module** | `hardy-tcpclv4` |
| **Target Directory** | `tcpclv4/fuzz/fuzz_targets/` |
| **Tooling** | `cargo fuzz` (libFuzzer) |
| **Test Suite ID** | FUZZ-TCPCL-01 |
| **Version** | 1.1 |

## 1. Introduction

This document details the fuzz testing strategy for the `hardy-tcpclv4` module. As the primary network transport for the router, this module must be robust against malicious peers sending malformed frames, protocol violations, or infinite streams of garbage.

**Primary Objective:** Ensure the TCPCL session handler never panics, hangs, or allocates excessive memory when processing untrusted network input.

## 2. Fuzz Target Definitions

The strategy utilizes two complementary fuzz targets located in `tcpclv4/fuzz/fuzz_targets/`.

### 2.1 Target A: Protocol Stream — Passive (Listener)

* **Source File:** `passive.rs`
* **Status:** Implemented

* **Input:** Random byte stream (`&[u8]`).

* **Harness:**

  1. Spawn a real TCPCLv4 CLA listener on loopback with a mock BPA (once, at process start).

  2. Connect to the listener via TCP for each fuzz iteration.

  3. Write the random input bytes and close the connection.

* **Goal:** Verify that the **Parser** and **State Machine** handle adversarial client input:

  * Partial/Fragmented headers.
  * Invalid Magic Headers.
  * Unexpected message types (e.g., `XFER_ACK` before `SESS_INIT`).
  * Huge length fields (OOM protection).

### 2.2 Target A: Protocol Stream — Active (Connector)

* **Source File:** `active.rs`
* **Status:** Implemented

* **Input:** Random byte stream (`&[u8]`).

* **Harness:**

  1. Create a TCPCLv4 CLA with no listener and a mock BPA (once, at process start).

  2. Bind a fake server on loopback (once, at process start).

  3. Trigger `cla.connect()` for each fuzz iteration.

  4. Accept the connection and write fuzz bytes as the "server" response.

* **Goal:** Verify that the **Connector** handles adversarial server responses:

  * Garbage contact headers.
  * Invalid SESS_INIT responses.
  * Unexpected messages during handshake.

### 2.3 Target B: Service Logic (Structured)

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
# Target A — Passive: Listener (Raw Bytes)
cargo +nightly fuzz run passive -- -max_total_time=1800

# Target A — Active: Connector (Raw Bytes)
cargo +nightly fuzz run active -- -max_total_time=1800

# Target B: Service Logic (Structs) — not yet implemented
# cargo +nightly fuzz run service_logic -- -max_total_time=1800
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

## 6. Shared Infrastructure

* **Location:** `tcpclv4/fuzz/src/lib.rs`
* **Provides:** `MockSink`, `MockBpa`, `setup_listener()`, `setup_connector()`, `FUZZ_ADDR`
* **Session config:** 2s contact timeout, no keepalive, no TLS (tuned for fuzz throughput)

## 7. Corpus Management

* **Location:** `tcpclv4/fuzz/corpus/{target_name}/`
* **Seed Data** (to be created):
  * **Passive:** Hex dump of a valid client contact header (`dtn!\x04\x00`).
  * **Active:** Hex dump of a valid server contact header response.
  * **Service:** Serialized sequence of `SessInit`, `XferSegment` structs.
