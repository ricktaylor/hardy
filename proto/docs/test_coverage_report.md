# Proto Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-proto` |
| **Crate version** | `0.2.0` |
| **Standard** | — |
| **Test Plans** | [`COMP-GRPC-01`](component_test_plan.md) |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `hardy-proto` crate has no formal LLRs — it is internal gRPC infrastructure. The table below maps functional areas to their verification status. All functional areas verified (7 pass).

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| **—** | Application Client registration and message flow | Pass | `app_cli_01..06` | — |
| **—** | CLA Client registration and message flow | Pass | `cla_cli_01..05` | — |
| **—** | Service Client registration and message flow | Pass | `svc_cli_01..05` | — |
| **—** | Routing Agent Client registration and message flow | Pass | `rte_cli_01..04` | — |
| **—** | Error handling (premature close, duplicate response, invalid sequence) | Pass | `err_cli_02..04` | — |
| **—** | Unregistration and lifecycle (client-init, BPA-init, crash, simultaneous, exactly-once) | Pass | `life_01..06` | — |
| **—** | Server proxy handlers (route request translation and delegation) | Pass | `srv_02..04` | — |

## 2. Test Inventory

### Suite 1: Application Client (`tests/application_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `app_cli_01_registration_ipn` | APP-CLI-01 | Register application (IPN), verify the endpoint the service is built with |
| `app_cli_02_registration_dtn` | APP-CLI-02 | Register application (DTN service name) |
| `app_cli_03_send_payload` | APP-CLI-03 | Send payload via sink, verify round-trip (mock returns error) |
| `app_cli_04_receive_payload` | APP-CLI-04 | Receive payload (BPA→App) via server-side push |
| `app_cli_05_status_notify` | APP-CLI-05 | Status notification (BPA→App) via server-side push |

### Suite 2: CLA Client (`tests/cla_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `cla_cli_01_registration` | CLA-CLI-01 | Register CLA with address type, verify the node IDs the CLA is built with |
| `cla_cli_02_dispatch_bundle` | CLA-CLI-02 | Dispatch bundle (CLA→BPA) via sink round-trip |
| `cla_cli_03_forward_bundle` | CLA-CLI-03 | Forward bundle (BPA→CLA) via server-side push to client |
| `cla_cli_04_add_peer` | CLA-CLI-04 | Add peer via sink round-trip |
| `cla_cli_05_remove_peer` | CLA-CLI-05 | Remove peer via sink round-trip |

### Suite 3: Service Client (`tests/service_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `svc_cli_01_registration` | SVC-CLI-01 | Register service with IPN service ID, verify the endpoint the service is built with |
| `svc_cli_02_send_bundle` | SVC-CLI-02 | Send raw bundle via sink, verify error from mock BPA |
| `svc_cli_03_receive_bundle` | SVC-CLI-03 | Receive bundle (BPA→Service) via server-side push to client |
| `svc_cli_04_status_notify` | SVC-CLI-04 | Status notification (BPA→Service) via server-side push to client |

### Suite 4: Routing Agent Client (`tests/routing.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `rte_cli_01_registration` | RTE-CLI-01 | Register routing agent via gRPC, verify the node IDs the agent is built with |
| `rte_cli_02_add_route` | RTE-CLI-02 | Add route via sink, verify round-trip through gRPC proxy |
| `rte_cli_03_remove_route` | RTE-CLI-03 | Remove route via sink, verify round-trip through gRPC proxy |
| `rte_cli_04_refused_registration_reports_its_status` | RTE-CLI-04 | A registration the BPA refuses reaches the client as the status the BPA's error maps to |

### Suite 5: Error Handling (unit tests in `src/client/routing.rs`)

These are unit tests because they use custom mock servers built from crate-internal proto types. ERR-CLI-01 (Connection Refused) was removed — it only exercised tonic's transport error path, not Hardy-specific logic.

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `err_cli_02_premature_stream_end` | ERR-CLI-02 | Custom server closes stream after handshake, verify the agent is shut down |
| `err_cli_03_duplicate_register_response` | ERR-CLI-03 | Custom server sends duplicate RegisterResponse, verify no panic |
| `err_cli_04_invalid_message_sequence` | ERR-CLI-04 | Custom server sends out-of-sequence msg during handshake, verify error |

### Suite 6: Unregistration & Lifecycle (`tests/lifecycle_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `life_01_client_initiated_unregister` | LIFE-01 | Client calls `sink.unregister()`, server on_close unregisters the BPA sink, client shuts its agent down |
| `life_02_bpa_initiated_unregister` | LIFE-02 | BPA shuts the server agent down, proxy shuts down, client shuts its agent down |
| `life_04_server_crash` | LIFE-04 | Server forces unregistration, client shuts its agent down via on_close |
| `life_05_simultaneous_unregister` | LIFE-05 | Client and BPA unregister concurrently, Mutex take() ensures no double shutdown or deadlock |
| `life_06_exactly_once_unregister` | LIFE-06 | BPA-initiated unregister shuts the client agent down exactly once |

LIFE-03 (drop without unregister) was removed: the session owns the agent, the agent owns the sink, and the sink owns the proxy, so a client can no longer drop the proxy without unregistering.

### Suite 7: Server Proxy Handlers (unit tests in `src/server/routing.rs`)

These are unit tests on the server-side `Agent`, the BPA's end of a remote routing agent. SRV-01 (Registration handshake), SRV-05 (on_close cancels proxy), and SRV-06 (on_unregister drains proxy) were removed — they are covered by Suites 1–4 and Suite 6 lifecycle tests respectively.

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `srv_02_added_route_reaches_the_sink` | SRV-02 | An AddRouteRequest is translated and handed to the BPA sink |
| `srv_03_removed_route_reaches_the_sink` | SRV-03 | A RemoveRouteRequest withdraws the route from the BPA sink |
| `srv_04_malformed_route_request_is_refused` | SRV-04 | An unparsable pattern fails InvalidArgument and leaves the sink untouched |

## 3. Coverage vs Plan

| Suite | Area | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| 1 | Application Client | 6 | 6 | **Complete** |
| 2 | CLA Client | 5 | 5 | **Complete** |
| 3 | Service Client | 5 | 5 | **Complete** |
| 4 | Routing Agent Client | 4 | 4 | **Complete** |
| 5 | Error Handling | 3 | 3 | **Complete** |
| 6 | Unregistration & Lifecycle | 5 | 5 | **Complete** |
| 7 | Server Proxy Handlers | 3 | 3 | **Complete** |
| | **Total** | **31** | **31** | **100%** |

## 4. Line Coverage

> Current figures are generated — see the [coverage summary](../../docs/coverage_summary.md) (refreshed by `scripts/run_lcov.sh`) and the live coverage dashboards (CFLite fuzz coverage on gh-pages; CI-published coverage planned). The snapshot below is from the crate version in the header.

```
cargo llvm-cov test --package hardy-proto --lcov --output-path lcov.info
lcov --summary lcov.info
```

```
  lines......: 80.4% (1280 of 1593 lines)
  functions..: 23.5% (400 of 1705 functions)
```

| File | Lines | Coverage |
| :--- | :--- | :--- |
| `client/application.rs` | 100 / 175 | 57.1% |
| `client/cla.rs` | 83 / 116 | 71.6% |
| `client/mod.rs` | 16 / 20 | 80.0% |
| `client/routing.rs` | 180 / 205 | 87.8% |
| `client/service.rs` | 94 / 135 | 69.6% |
| `proto.rs` | 142 / 192 | 74.0% |
| `proxy.rs` | 175 / 195 | 89.7% |
| `server/application.rs` | 114 / 160 | 71.3% |
| `server/cla.rs` | 138 / 167 | 82.6% |
| `server/mod.rs` | 44 / 56 | 78.6% |
| `server/routing.rs` | 158 / 179 | 88.3% |
| `server/service.rs` | 110 / 136 | 80.9% |
| **Total** | **1259 / 1566** | **80.4%** |

_Per-file figures are from a previous detailed run; regenerate with `cargo llvm-cov --html`._

The lowest-covered files are the Application and Service client proxies — their server-push callbacks (`on_deliver`, `on_status_notify`) have error-handling branches that the mock BPA doesn't exercise. The core proxy infrastructure (`proxy.rs`, 89.7%) and routing modules (`client/routing.rs`, 87.8%; `server/routing.rs`, 88.3%) have the highest coverage due to the error handling and server proxy unit tests.

## 5. Test Infrastructure

### Mock Framework (`tests/common/`)

| File | Purpose |
| :--- | :--- |
| `common/mod.rs` | MockBpa (BpaRegistration impl), sink wrappers, server helpers, port allocation |
| `common/sinks.rs` | Mock sink implementations (`routing::Sink`, `cla::Sink`, `service::raw::Sink`, `service::Sink`) |

### Key Bugs Found During Development

| Bug | Root Cause | Fix |
| :--- | :--- | :--- |
| Client WARN "Failed to request unregistration" | Server cancelled proxy before Unregister response was sent | Removed Unregister/OnUnregister messages; unregistration via stream close |
| BPA hang on shutdown (force kill) | Spin lock held across `.await` in `if let Some(sink) = self.sink.lock().take()` | Split into `let sink = self.sink.lock().take();` then `sink.unregister().await` |
| Orphaned proxy tasks on drop | RpcProxy Drop didn't cancel tasks | Added `Drop` impl that calls `cancel()` |
| Re-entrant shutdown deadlock | `proxy.shutdown()` called from within reader task (on_close) | Added `is_cancelled()` guard in `shutdown()` |

## 6. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Application/Service client proxies | Server-push error-handling branches uncovered | Low | Error paths in `on_deliver`, `on_status_notify` callbacks |

## 7. Conclusion

The `hardy-proto` crate has 31 tests covering all 7 functional areas at 100% plan coverage. Line coverage is 80.4% (1280/1593 lines). The core proxy infrastructure and routing modules have the highest coverage (87-90%), while Application and Service client proxies are lower (57-70%) due to untested server-push error branches. The lifecycle test suite (5 tests) provides thorough verification of the unregistration protocol including concurrent and crash scenarios.
