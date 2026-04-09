# TCPCLv4 Server Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-tcpclv4-server` |
| **Test Plan** | [`PLAN-TCPCL-SERVER-01`](test_plan.md) |
| **Date** | 2026-04-09 |

## 1. Coverage Summary

| Suite | Area | Planned | Covered | Status |
| :--- | :--- | :--- | :--- | :--- |
| §3 CFG-01–03 | Configuration logic | 3 | 0 | **Not started** |
| §4.1 SYS-01–03 | Lifecycle & integration | 3 | 3 | **Covered** (interop + CI) |
| §4.2 OTEL-01–03 | Observability | 3 | 0 | **Not started** |
| §4.3 PERF-SRV-01 | Performance | 1 | 0 | **Not started** |
| §4.4 PKG-OCI/HELM | Packaging | 2 | 0 | **Not started** |
| UT-TCP-01–05 | Library unit tests | 5 | 2 | **Partial** (fuzz only) |
| TCP-01–10 | Library component tests | 10 | 0 | **Not started** |
| FUZZ-TCPCL-01 | Library fuzz targets | 2 | 2 | **Complete** (on branch) |
| Interop | End-to-end scripts | 4 | 4 | **Complete** |
| CI pipeline | Container ping test | 1 | 1 | **Complete** |
| | **Total** | **34** | **12** | **35%** |

## 2. Server-Specific Tests

No server-specific unit or system tests are implemented. All planned tests in the test plan (CFG, OTEL, PERF, PKG) remain at 0%.

## 3. Cross-Coverage from Other Test Suites

The tcpclv4-server binary is a thin wrapper around the `hardy-tcpclv4` library. The server-specific code (`main.rs`, `config.rs`) is ~150 lines of application wiring. Protocol-level coverage is provided by library and integration test suites.

### 3.1 Coverage from `hardy-tcpclv4` Library

| Area | Status | Notes |
| :--- | :--- | :--- |
| **UT-TCP-01** Message SerDes | Exercised by fuzz | Codec encode/decode of all TCPCL message types |
| **UT-TCP-02** Contact Header | Exercised by fuzz | Magic string and version validation |
| **UT-TCP-03** Parameter Negotiation | Stub | `session.rs:655` — not implemented |
| **UT-TCP-04** Fragment Logic | Stub | `session.rs:663` — not implemented |
| **UT-TCP-05** Reason Codes | Stub | `session.rs:670` — not implemented |
| **FUZZ-TCPCL-01** Passive Listener | Complete | `fuzz_targets/passive.rs` (on `tcpclv4-fuzz` branch) |
| **FUZZ-TCPCL-01** Active Connector | Complete | `fuzz_targets/active.rs` (on `tcpclv4-fuzz` branch) |

Fuzz testing found one bug: subtraction overflow in `codec.rs` extension parsing (SESS_INIT + XFER_SEGMENT). Fixed with `saturating_sub`/`saturating_add` on the `tcpclv4-fuzz` branch.

### 3.2 Coverage from Interop Tests

| Test Script | Peer | Scenarios exercised |
| :--- | :--- | :--- |
| `hardy/test_hardy_ping.sh` | Hardy | Startup, BPA registration, active+passive handshake, bidirectional transfer, graceful shutdown |
| `dtn7-rs/test_dtn7rs_ping.sh` | dtn7-rs | Cross-impl TCPCLv4 handshake, extension tolerance, transfer |
| `HDTN/test_hdtn_ping.sh` | HDTN | Cross-impl TCPCLv4 handshake, transfer |
| `DTNME/test_dtnme_ping.sh` | DTNME | Cross-impl TCPCLv4 handshake, transfer |

### 3.3 Coverage from CI Pipeline

| Test | Scenarios exercised |
| :--- | :--- |
| `compose.ping-tests.yml` | Docker container startup, health check, `bp ping` loopback, graceful shutdown via container stop |

### 3.4 Server Scenario Coverage Matrix

| Server scenario | Test plan ref | Covered by | Formal test? |
| :--- | :--- | :--- | :--- |
| Process startup | SYS-01 | Interop + CI | Implicit |
| TCP listen on configured port | SYS-01 | Interop + CI | Implicit |
| BPA gRPC registration | SYS-02 | Interop + CI | Implicit |
| Graceful shutdown (SIGTERM) | SYS-03 | Interop + CI | Implicit |
| Active TCPCLv4 handshake | TCP-01 (library) | Interop (4 implementations) | Yes |
| Passive TCPCLv4 handshake | TCP-01 (library) | Interop (4 implementations) | Yes |
| Bundle transfer (send + receive) | TCP-03 (library) | Interop + CI | Yes |
| Configuration defaults | CFG-01 | — | **Not covered** |
| Configuration file parsing | CFG-02 | — | **Not covered** |
| Environment variable overrides | CFG-03 | — | **Not covered** |
| OTEL trace export | OTEL-01 | — | **Not covered** |
| OTEL metrics export | OTEL-02 | — | **Not covered** |
| OTEL log export | OTEL-03 | — | **Not covered** |
| mTLS | — | — | **Not implemented** |
| OCI image structure | PKG-OCI-01 | — | **Not covered** |
| Helm chart | PKG-HELM-01 | — | **Not covered** |

## 4. Known Gaps

| Gap | Impact | Priority |
| :--- | :--- | :--- |
| No configuration tests (CFG-01–03) | Config errors only caught at runtime | Medium |
| No OTEL metrics defined or tested | No visibility into CLA performance | Medium |
| Library unit test stubs (UT-TCP-03–05) | Parameter negotiation, fragmentation, and reason code logic untested at unit level | Medium |
| Fuzz targets not merged to main | `tcpclv4-fuzz` branch with codec bug fix | High — merge before release |
| mTLS not implemented | `config.rs:45`, `context.rs:352`, `connect.rs:163` | Deferred |

## 5. Conclusion

The tcpclv4-server has effective protocol-level coverage through library fuzz targets and interop testing against four DTN implementations. The three lifecycle scenarios (startup, registration, shutdown) are implicitly exercised by every interop and CI test run. Server-specific concerns — configuration parsing, OTEL observability, and packaging verification — have no formal tests. The highest-priority gap is merging the `tcpclv4-fuzz` branch.
