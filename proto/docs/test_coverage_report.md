# Proto Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-proto` |
| **Test Plan** | [`COMP-GRPC-01`](component_test_plan.md) |
| **Date** | 2026-04-01 |

## 1. Coverage Summary

| Suite | Area | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| 1 | Application Client | 6 | 6 | **Complete** |
| 2 | CLA Client | 5 | 5 | **Complete** |
| 3 | Service Client | 5 | 5 | **Complete** |
| 4 | Routing Agent Client | 3 | 3 | **Complete** |
| 5 | Error Handling | 3 | 3 | **Complete** |
| 6 | Unregistration & Lifecycle | 6 | 6 | **Complete** |
| 7 | Server Proxy Handlers | 6 | 0 | Not started |
| | **Total** | **34** | **28** | **82%** |

## 2. Test Inventory

### Suite 1: Application Client (`tests/application_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `app_cli_01_registration_ipn` | APP-CLI-01 | Register application (IPN), verify endpoint and on_register |
| `app_cli_02_registration_dtn` | APP-CLI-02 | Register application (DTN service name) |
| `app_cli_03_send_payload` | APP-CLI-03 | Send payload via sink, verify round-trip (mock returns error) |
| `app_cli_04_receive_payload` | APP-CLI-04 | Receive payload (BPA→App) via server-side push |
| `app_cli_05_status_notify` | APP-CLI-05 | Status notification (BPA→App) via server-side push |
| `app_cli_06_cancel` | APP-CLI-06 | Cancel pending send via sink round-trip |

### Suite 2: CLA Client (`tests/cla_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `cla_cli_01_registration` | CLA-CLI-01 | Register CLA with address type, verify node IDs and on_register |
| `cla_cli_02_dispatch_bundle` | CLA-CLI-02 | Dispatch bundle (CLA→BPA) via sink round-trip |
| `cla_cli_03_forward_bundle` | CLA-CLI-03 | Forward bundle (BPA→CLA) via server-side push to client |
| `cla_cli_04_add_peer` | CLA-CLI-04 | Add peer via sink round-trip |
| `cla_cli_05_remove_peer` | CLA-CLI-05 | Remove peer via sink round-trip |

### Suite 3: Service Client (`tests/service_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `svc_cli_01_registration` | SVC-CLI-01 | Register service with IPN service ID, verify endpoint and on_register |
| `svc_cli_02_send_bundle` | SVC-CLI-02 | Send raw bundle via sink, verify error from mock BPA |
| `svc_cli_03_receive_bundle` | SVC-CLI-03 | Receive bundle (BPA→Service) via server-side push to client |
| `svc_cli_04_status_notify` | SVC-CLI-04 | Status notification (BPA→Service) via server-side push to client |
| `svc_cli_05_cancel` | SVC-CLI-05 | Cancel pending send via sink round-trip |

### Suite 4: Routing Agent Client (`tests/routing_agent_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `rte_cli_01_registration` | RTE-CLI-01 | Register routing agent via gRPC, verify node IDs and on_register callback |
| `rte_cli_02_add_route` | RTE-CLI-02 | Add route via sink, verify round-trip through gRPC proxy |
| `rte_cli_03_remove_route` | RTE-CLI-03 | Remove route via sink, verify round-trip through gRPC proxy |

### Suite 5: Error Handling (unit tests in `src/client/routing.rs`)

These are unit tests because they use custom mock servers built from crate-internal proto types. ERR-CLI-01 (Connection Refused) was removed — it only exercised tonic's transport error path, not Hardy-specific logic.

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `err_cli_02_premature_stream_end` | ERR-CLI-02 | Custom server closes stream after handshake, verify synthetic on_unregister |
| `err_cli_03_duplicate_register_response` | ERR-CLI-03 | Custom server sends duplicate RegisterResponse, verify no panic |
| `err_cli_04_invalid_message_sequence` | ERR-CLI-04 | Custom server sends out-of-sequence msg during handshake, verify error |

### Suite 6: Unregistration & Lifecycle (`tests/lifecycle_tests.rs`)

| Test Function | Test ID | Scope |
| :--- | :--- | :--- |
| `life_01_client_initiated_unregister` | LIFE-01 | Client calls `sink.unregister()`, server on_close cleans up, client receives synthetic on_unregister |
| `life_02_bpa_initiated_unregister` | LIFE-02 | BPA calls on_unregister on server agent, proxy shuts down, client receives synthetic on_unregister |
| `life_03_drop_without_unregister` | LIFE-03 | Client drops sink without unregister, RpcProxy Drop cancels tasks, server on_close cleans up |
| `life_04_server_crash` | LIFE-04 | Server forces unregistration, client receives synthetic on_unregister via on_close |
| `life_05_simultaneous_unregister` | LIFE-05 | Client and BPA unregister concurrently, Mutex take() ensures no double-unregister or deadlock |
| `life_06_exactly_once_unregister` | LIFE-06 | BPA-initiated unregister delivers exactly one on_unregister to client trait impl |

### Test Infrastructure (`tests/common/`)

| File | Purpose |
| :--- | :--- |
| `common/mod.rs` | MockBpa (BpaRegistration impl), sink wrappers, server helpers, port allocation |
| `common/sinks.rs` | Mock sink implementations (RoutingSink, CLA Sink, ServiceSink, ApplicationSink) |

## 3. Key Bugs Found During Development

| Bug | Root Cause | Fix |
| :--- | :--- | :--- |
| Client WARN "Failed to request unregistration" | Server cancelled proxy before Unregister response was sent | Removed Unregister/OnUnregister messages; unregistration via stream close |
| BPA hang on shutdown (force kill) | Spin lock held across `.await` in `if let Some(sink) = self.sink.lock().take()` | Split into `let sink = self.sink.lock().take();` then `sink.unregister().await` |
| Orphaned proxy tasks on drop | RpcProxy Drop didn't cancel tasks | Added `Drop` impl that calls `cancel()` |
| Re-entrant shutdown deadlock | `proxy.shutdown()` called from within reader task (on_close) | Added `is_cancelled()` guard in `shutdown()` |

## 4. Gaps

- **Suite 7** (Server proxy handlers): sink ownership lifecycle, spin lock safety not yet unit tested. Covered implicitly by Suite 6 lifecycle tests.
- **Line coverage**: not yet measured (`cargo llvm-cov` not available in container).
