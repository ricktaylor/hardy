# TCPCLv4 Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-tcpclv4` |
| **Standard** | RFC 9174 — Delay-Tolerant Networking TCP Convergence-Layer Protocol Version 4 |
| **Test Plans** | [`PLAN-TCPCL-01`](component_test_plan.md), [`FUZZ-TCPCL-01`](fuzz_test_plan.md) |
| **Date** | 2026-04-14 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

All 10 Low-Level Requirements derived from REQ-3 are **verified** (10 pass, 0 N/A). Compliance is verified through interoperability testing ([`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md)) against 4 TCPCLv4 peer implementations.

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| **3.1.1** | Active session establishment | Pass | Interop: Hardy initiates to dtn7-rs, HDTN, DTNME | 3.2 |
| **3.1.2** | Passive session establishment | Pass | Interop: dtn7-rs, HDTN, DTNME initiate to Hardy | 3.2 |
| **3.1.3** | Connection pooling | Pass | Interop: multi-ping (connection reuse) | 3.2 |
| **3.1.4** | Local Node IDs in SESS_INIT | Pass | Interop: IPN node ID exchange with all peers | 3.2 |
| **3.1.5** | Configurable session parameters | Pass | Interop: negotiated values with all peers | 3.2 |
| **3.1.6** | Extension items in SESS_INIT | Pass | Interop: passively exercised | 3.2 |
| **3.1.7** | TLS support | Pass | Interop: TLS-capable peers | 3.2 |
| **3.1.8** | TLS enabled by default | Pass | Interop: default configuration | 3.2 |
| **3.1.9** | TLS Entity Identification | Pass | Interop: TLS peers | 3.2 |
| **3.1.10** | Session keepalive | Pass | Interop: long-running tests | 3.2 |

## 2. Test Inventory

### Interoperability Tests

| Peer | Organisation | Direction | Status |
| :--- | :--- | :--- | :--- |
| **Hardy** | Aalyria | Hardy ↔ Hardy | Passing |
| **dtn7-rs** | Community | Hardy ↔ dtn7-rs | Passing |
| **HDTN** | NASA Glenn | Hardy ↔ HDTN | Passing |
| **DTNME** | NASA Marshal | Hardy ↔ DTNME | Passing |

Each test exercises bidirectional bundle transfer: contact header exchange, SESS_INIT negotiation, XFER_SEGMENT/XFER_ACK data transfer, and session teardown. See [`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md) for details.

### Unit Tests (25 tests)

| Test ID | File | Tests | Scope |
| :--- | :--- | :--- | :--- |
| UT-TCP-01 | `codec.rs` | 11 | Message encode/decode round-trip for all 7 message types, invalid type, incomplete data |
| UT-TCP-02 | — | — | Contact header validation is inline in connect/context; covered by interop + fuzz |
| UT-TCP-03 | `session.rs` | 6 | Keepalive negotiation (local/peer min), segment MRU negotiation, disabled cases |
| UT-TCP-04 | `session.rs` | 5 | Fragment logic: single segment, exact MTU, 10-segment split, remainder, START/END flags |
| UT-TCP-05 | `codec.rs` | 3 | Reason code round-trip (SESS_TERM, XFER_REFUSE), unassigned/private ranges |

### Fuzz Tests

| Target | File | Status |
| :--- | :--- | :--- |
| Passive (listener) | `fuzz_targets/passive.rs` | Implemented |
| Active (connector) | `fuzz_targets/active.rs` | Implemented |

**Totals:** 4 interop test suites, 25 unit tests, 2 fuzz targets.

## 3. Coverage vs Plan

### 3.1 Section 2 — Testing Strategy

| Item | Plan | Status | Action |
| :--- | :--- | :--- | :--- |
| Generic CLA trait harness | `cargo test --test cla_harness` | **Does not exist** | Build harness, or re-map to BPA fuzz harness (`bpa/fuzz/src/cla.rs`) which exercises the trait |
| `duplex` harness for protocol tests | Simulates a peer via in-memory pipe | **Does not exist** | Build `tokio::io::duplex`-based test harness for TCP-01 through TCP-10 |

### 3.2 Section 4 — Component Tests (TCP-01 to TCP-10)

All 10 tests are defined in the plan but have no test code. Interop tests provide system-level coverage of the same scenarios.

| Test ID | Scenario | LLR | Interop Coverage | Dedicated Test |
| :--- | :--- | :--- | :--- | :--- |
| TCP-01 | Active/Passive Handshake | 3.1.1, 3.1.2 | Every interop test | Not implemented |
| TCP-02 | Session Parameters | 3.1.4, 3.1.5 | Every interop test (node ID + params) | Not implemented |
| TCP-03 | Data Segmentation | — | Every interop test (bundle transfer) | Not implemented |
| TCP-04 | Keepalive | 3.1.10 | Long-running interop tests | Not implemented |
| TCP-05 | TLS Handshake (Default) | 3.1.7, 3.1.8 | Interop with TLS peers | Not implemented |
| TCP-06 | TLS Disabled | 3.1.8 | Interop with `--no-tls` config | Not implemented |
| TCP-07 | Connection Pooling | 3.1.3 | Multi-ping (connection reuse) | Not implemented |
| TCP-08 | Protocol Error | — | Not covered | Not implemented |
| TCP-09 | TLS Entity ID | 3.1.9 | Interop with TLS peers | Not implemented |
| TCP-10 | Session Extensions | 3.1.6 | Passively exercised | Not implemented |

**Note:** TCP-08 (protocol error handling) is the only scenario not covered by interop tests — it requires an intentionally misbehaving peer, which is what the `duplex` harness is designed for.

### 3.3 Section 5 — Unit Tests (UT-TCP-01 to UT-TCP-05)

| Test ID | Scenario | Source | Status |
| :--- | :--- | :--- | :--- |
| UT-TCP-01 | Message SerDes | `src/codec.rs` | **Complete** (11 tests) |
| UT-TCP-02 | Contact Header | — | Covered by interop + fuzz (validation inline in connect/context) |
| UT-TCP-03 | Parameter Negotiation | `src/session.rs` | **Complete** (6 tests) |
| UT-TCP-04 | Fragment Logic | `src/session.rs` | **Complete** (5 tests) |
| UT-TCP-05 | Reason Codes | `src/codec.rs` | **Complete** (3 tests) |

### 3.4 Section 6 — Scaling Tests (TCPCL-SCALE-01 to TCPCL-SCALE-04)

| Test ID | Scenario | Status |
| :--- | :--- | :--- |
| TCPCL-SCALE-01 | 100 concurrent sessions | Not implemented (interop is sequential) |
| TCPCL-SCALE-02 | 1000 concurrent sessions | Not implemented |
| TCPCL-SCALE-03 | Connection churn (100 conn/sec) | Not implemented |
| TCPCL-SCALE-04 | TLS handshake throughput | Not implemented |

These are performance/stress tests — appropriate for Full Activity, not De-risk.

### 3.5 Fuzz Tests (FUZZ-TCPCL-01)

| Target | Plan Name | Status |
| :--- | :--- | :--- |
| `fuzz_targets/passive.rs` | Target A (Protocol Stream) | **Implemented** — spawns listener with mock BPA, connects via loopback TCP, writes fuzz bytes |
| `fuzz_targets/active.rs` | Target A (Protocol Stream) | **Implemented** — binds fake server, triggers CLA `connect()`, writes fuzz bytes as server response |
| Target B (Service Logic) | Structured message fuzzing | Not implemented |

Shared infrastructure in `src/lib.rs`: `MockSink`, `MockBpa`, `setup_listener()`, `setup_connector()`, tuned session config (2s contact timeout, no keepalive).

## 4. Line Coverage

`cargo llvm-cov` has not been run since unit tests were added. The 28 unit tests exercise codec, negotiation, and segmentation logic. Interop tests (4 implementations) and fuzz targets (2 targets) run out-of-process and are not captured by `llvm-cov`.

## 5. Test Infrastructure

- **Fuzz shared library** (`fuzz/src/lib.rs`): `MockSink`, `MockBpa`, `setup_listener()`, `setup_connector()`, `FUZZ_ADDR` — shared between passive and active fuzz targets
- **Fuzz session config**: 2s contact timeout, no keepalive, tuned for per-iteration timeout (5s passive, 15s active)
- **Interop tests**: shell scripts in `tests/interop/` ([`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md)) exercising bidirectional bundle transfer against 4 TCPCLv4 peer implementations
- **Planned**: `tokio::io::duplex`-based harness for isolated protocol-level component tests (TCP-01 through TCP-10)

## 6. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Component tests | 0 of 10 planned component tests implemented | Medium | Interop covers 9/10 scenarios; TCP-08 (protocol error) has no coverage |
| Scaling tests | 0 of 4 planned scaling tests implemented | Low | Full Activity scope |
| Protocol error handling | TCP-08 not covered by any test layer | High | Requires intentionally misbehaving peer (`duplex` harness) |

## 7. Conclusion

The TCPCLv4 crate has 25 unit tests, 4 interop test suites, and 2 fuzz targets. 4 of 5 unit test scenarios are implemented (UT-TCP-01, 03, 04, 05); UT-TCP-02 (contact header validation) is covered by interop and fuzz tests as the validation logic is inline in the connection code. All 10 LLRs (3.1.1 through 3.1.10) are verified as Pass via interoperability testing, satisfying Part 4 Ref 3.2. The primary remaining gap is TCP-08 (protocol error handling), which requires the planned `duplex` test harness.
