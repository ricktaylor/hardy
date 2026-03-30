# TCPCLv4 Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-tcpclv4` + `hardy-tcpclv4-server` |
| **Standard** | RFC 9174 — Delay-Tolerant Networking TCP Convergence-Layer Protocol Version 4 |
| **Test Plan** | [`PLAN-TCPCL-01`](component_test_plan.md) |
| **Date** | 2026-03-30 |

## 1. RFC 9174 Compliance Summary

All 10 Low-Level Requirements derived from REQ-3 are **implemented**. Compliance is currently verified through interoperability testing against 3 independent TCPCLv4 implementations.

| LLR | Feature | RFC 9174 | Implementation | Interop Evidence |
| :--- | :--- | :--- | :--- | :--- |
| **3.1.1** | Active session establishment | §3 | `connect.rs` — contact header, SESS_INIT, 5 retries | Hardy initiates to dtn7-rs, HDTN, DTNME |
| **3.1.2** | Passive session establishment | §3 | `listen.rs`, `context.rs` — accept, negotiate | dtn7-rs, HDTN, DTNME initiate to Hardy |
| **3.1.3** | Connection pooling | — | `connection.rs` — idle pool per address, max_idle config | Multi-ping benchmark (connection reuse) |
| **3.1.4** | Local Node IDs in SESS_INIT | §4.6 | `codec.rs`, `context.rs` — type-matched ID selection | IPN node ID exchange with all peers |
| **3.1.5** | Configurable session parameters | §4.7 | `config.rs` — timeout, keepalive, MRU; `context.rs` — min negotiation | Negotiated values with all peers |
| **3.1.6** | Extension items in SESS_INIT | §4.8 | `codec.rs` — critical flag handling, non-critical ignored | Passively exercised |
| **3.1.7** | TLS support | §4.4 | `tls.rs` (350 lines) — rustls server/client, cert loading | TLS-capable peers |
| **3.1.8** | TLS enabled by default | §4.4 | `lib.rs` — TLS offered if configured, `require_tls` enforcement | Default configuration |
| **3.1.9** | TLS Entity Identification | §4.4.1 | `connect.rs` — DNS SNI + IP fallback; `tls.rs` — CA validation | TLS peers |
| **3.1.10** | Session keepalive | — | `writer.rs` — independent task; `session.rs` — 2x receive timeout | Long-running benchmark |

## 2. Interoperability Evidence

| Peer | Organisation | Direction | Status |
| :--- | :--- | :--- | :--- |
| **Hardy** | Aalyria | Hardy ↔ Hardy | Passing |
| **dtn7-rs** | Community | Hardy ↔ dtn7-rs | Passing |
| **HDTN** | NASA Glenn | Hardy ↔ HDTN | Passing |
| **DTNME** | NASA Marshal | Hardy ↔ DTNME | Passing |

Each test exercises bidirectional bundle transfer: contact header exchange, SESS_INIT negotiation, XFER_SEGMENT/XFER_ACK data transfer, and session teardown.

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
| TCP-04 | Keepalive | 3.1.10 | Long-running benchmarks | Not implemented |
| TCP-05 | TLS Handshake (Default) | 3.1.7, 3.1.8 | Interop with TLS peers | Not implemented |
| TCP-06 | TLS Disabled | 3.1.8 | Interop with `--no-tls` config | Not implemented |
| TCP-07 | Connection Pooling | 3.1.3 | Multi-ping reuse | Not implemented |
| TCP-08 | Protocol Error | — | Not covered | Not implemented |
| TCP-09 | TLS Entity ID | 3.1.9 | Interop with TLS peers | Not implemented |
| TCP-10 | Session Extensions | 3.1.6 | Passively exercised | Not implemented |

**Note:** TCP-08 (protocol error handling) is the only scenario not covered by interop tests — it requires an intentionally misbehaving peer, which is what the `duplex` harness is designed for.

### 3.3 Section 5 — Unit Tests (UT-TCP-01 to UT-TCP-05)

| Test ID | Scenario | Source | Status |
| :--- | :--- | :--- | :--- |
| UT-TCP-01 | Message SerDes | `src/codec.rs` | **Not implemented** — encode/decode logic exists but no tests |
| UT-TCP-02 | Contact Header | `src/session.rs` | **Not implemented** — contact header logic in `connect.rs:41-93` |
| UT-TCP-03 | Parameter Negotiation | `src/session.rs` | **Stub** — commented out at `session.rs:653` |
| UT-TCP-04 | Fragment Logic | `src/session.rs` | **Stub** — commented out at `session.rs:661` |
| UT-TCP-05 | Reason Codes | `src/session.rs` | **Stub** — commented out at `session.rs:668` |

**Priority:** UT-TCP-01 (SerDes) is the highest-value unit test — it verifies wire format correctness without any I/O. UT-TCP-03 (negotiation) is trivial to implement (`min(local, peer)`).

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
| `fuzz_targets/passive.rs` | Target A (Protocol Stream) | **Implemented** — spawns real listener with mock BPA, connects via loopback TCP, writes fuzz bytes |
| `fuzz_targets/active.rs` | Target A (Protocol Stream) | **Implemented** — binds fake server, triggers CLA `connect()`, writes fuzz bytes as server response |
| Target B (Service Logic) | Structured message fuzzing | Not implemented |

Shared infrastructure in `src/lib.rs`: `MockSink`, `MockBpa`, `setup_listener()`, `setup_connector()`, tuned session config (2s contact timeout, no keepalive).

## 4. Summary

| Category | Planned | Implemented | Coverage |
| :--- | :--- | :--- | :--- |
| Component tests (TCP-xx) | 10 | 0 | Interop covers 9/10 scenarios |
| Unit tests (UT-TCP-xx) | 5 | 0 (3 stubs) | None |
| Scaling tests (TCPCL-SCALE-xx) | 4 | 0 | None (Full Activity) |
| Fuzz targets | 2 | 2 (passive + active) | Protocol stream parsing (both directions) |
| Interop peers (TCPCLv4) | — | 4 | Bidirectional, all passing |

## 5. Conclusion

The TCPCLv4 implementation has **full RFC 9174 feature coverage** across all 10 LLRs. Interoperability testing with 3 independent implementations (dtn7-rs, HDTN, DTNME) provides strong evidence of on-the-wire correctness. Fuzz testing covers both passive (listener) and active (connector) protocol paths with adversarial input. The remaining gaps are the planned `duplex` component test harness for isolated edge-case verification (particularly TCP-08: protocol error handling) and dedicated unit tests for wire format correctness (UT-TCP-01: message SerDes).
