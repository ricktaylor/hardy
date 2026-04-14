# Component Test Plan: Time-Variant Routing (TVR)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Contact Scheduling (gRPC Service, File Watcher, System Integration) |
| **Module** | `hardy-tvr` |
| **Requirements Ref** | [REQ-6](../../docs/requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth) |
| **Test Suite ID** | `COMP-TVR-01` |
| **Version** | 1.0 |

## 1. Introduction

This document details the component-level testing strategy for `hardy-tvr`. These tests verify the behaviour of the TVR gRPC session service and file watcher, which are not covered by the unit tests in [`UTP-TVR-01`](unit_test_plan.md).

**Scope:**

* gRPC session lifecycle: open, duplicate name rejection, message handling, stream close cleanup.

* File watcher: hot-reload, file deletion, parse error resilience.

* End-to-end route installation and withdrawal via the BPA.

**Note:** Proto→internal conversion functions (`convert_timestamp`, `convert_duration`, `convert_contact`) and their validation logic are covered by unit tests in [`UTP-TVR-01`](unit_test_plan.md) §3.22–3.24.

## 2. Testing Strategy

Component verification is split into two tiers:

1. **gRPC Session Tests:** Rust integration tests using a real `tonic` client connected to the TVR gRPC service over a local socket, with the scheduler backed by a mock `RoutingSink`. These verify session protocol correctness without requiring a running BPA.

2. **System Integration Tests:** Shell-scripted end-to-end tests (`tests/test_tvr.sh`) that start real BPA nodes, a hardy-tvr instance, and verify route installation and bundle forwarding via `bp ping`.

## 3. gRPC Session Tests

*Objective: Verify session lifecycle, message handling, and error paths
through the TVR gRPC service.*

These tests use `grpcurl` to interact with the TVR gRPC service directly
and are implemented in [`tests/test_tvr.sh`](../tests/test_tvr.sh)
alongside the system integration tests. This avoids the overhead of a
Rust tonic test harness while still exercising the session protocol
end-to-end.

| Test ID | Scenario | Shell Test | Status |
| :--- | :--- | :--- | :--- |
| **TVR-01** | **Session Open** — verify `OpenSessionResponse` returned. | TEST 5 | **Implemented** |
| **TVR-02** | **Duplicate Session Name** — second session rejected with `ALREADY_EXISTS`. | TEST 8 | **Implemented** |
| **TVR-03** | **Missing Open** — `AddContactsRequest` as first message rejected. | TEST 9 | **Implemented** |
| **TVR-04** | **Duplicate Open** — second open on same stream returns error. | — | Deferred (low risk) |
| **TVR-05** | **Add Contacts** — add permanent contacts, verify response counts. | TEST 6 | **Implemented** |
| **TVR-06** | **Add Invalid Contact** — missing action returns error. | — | Covered by unit test (§3.24) |
| **TVR-07** | **Remove Contacts** — add then remove, verify counts. | — | Deferred (low risk) |
| **TVR-08** | **Replace Contacts** — replace with overlapping set, verify diff counts. | — | Deferred (low risk) |
| **TVR-09** | **Stream Close Cleanup** — close stream, verify routes withdrawn. | TEST 7 | **Implemented** |
| **TVR-10** | **Cancellation** — cancel task pool, verify cleanup. | — | Implicit in test teardown |
| **TVR-11** | **Empty Message** — no `msg` oneof set, verify ignored. | — | Not testable via `grpcurl` |
| **TVR-12** | **Session Name Reuse After Close** — re-open with same name succeeds. | TEST 10 | **Implemented** |

Six of twelve scenarios are implemented via `grpcurl` in the shell
runner. The remaining scenarios are either covered by unit tests
(TVR-06), implicit in test teardown (TVR-10), not testable from a
well-behaved client (TVR-11), or low-risk straightforward code paths
(TVR-04, TVR-07, TVR-08) that can be added if the session protocol
grows in complexity.

## 4. System Integration Tests

*Objective: Verify end-to-end route installation, hot-reload, and bundle forwarding with real BPA nodes.*

These tests are implemented in [`tests/test_tvr.sh`](../tests/test_tvr.sh) and exercise the full stack: hardy-tvr → gRPC → BPA → TCPCLv4 → peer node.

**Architecture:**

```
┌──────────┐  gRPC   ┌───────────┐  TCPCLv4  ┌──────────┐
│ hardy-tvr│◄───────►│ BPA Node1 │◄─────────►│ BPA Node2│
│ (sched)  │ :50051  │ (routes)  │   :4560   │ (echo)   │
└──────────┘         └───────────┘           └──────────┘
```

| Test ID | Shell Test | Description | Pass Criteria |
| :--- | :--- | :--- | :--- |
| **TVR-SYS-01** | TEST 1 | **Permanent Route** — create contact plan, start hardy-tvr, ping Node 2. | `bp ping` succeeds. |
| **TVR-SYS-02** | TEST 2 | **Hot-Reload (Add)** — modify contact plan, ping original destination. | `bp ping` still succeeds. |
| **TVR-SYS-03** | TEST 3 | **File Removal** — delete contact plan, ping phantom node. | `bp ping` fails — routes withdrawn. |
| **TVR-SYS-04** | TEST 4 | **File Restore** — recreate contact plan, ping Node 2. | `bp ping` succeeds. |
| **TVR-SYS-05** | TEST 5 | **gRPC Session Open** — open session via `grpcurl`. | `OpenSessionResponse` received. |
| **TVR-SYS-06** | TEST 6 | **gRPC Add Contacts** — add contact via session, verify response. | `AddContactsResponse` with `added` count. |
| **TVR-SYS-07** | TEST 7 | **gRPC Close Cleanup** — close stream, re-add route in new session. | Re-add produces `active` count (route was withdrawn). |
| **TVR-SYS-08** | TEST 8 | **gRPC Duplicate Name** — second session with same name. | `ALREADY_EXISTS` error. |
| **TVR-SYS-09** | TEST 9 | **gRPC Missing Open** — add as first message. | `INVALID_ARGUMENT` error. |
| **TVR-SYS-10** | TEST 10 | **gRPC Name Reuse** — re-open after close with same name. | `OpenSessionResponse` received. |

## 5. Execution Strategy

* **All Tests:** `./tvr/tests/test_tvr.sh [--skip-build]`

* **Dependencies:** Built binaries (`hardy-bpa-server`, `hardy-tvr`, `bp`) and `grpcurl`.

* **Pass Criteria:** All 10 tests must pass.
