# Echo Service Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-echo-service` |
| **Standard** | [draft-taylor-dtn-echo-service](https://datatracker.ietf.org/doc/draft-taylor-dtn-echo-service/) |
| **Test Plans** | [`PLAN-ECHO-01`](test_plan.md), [`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md) |
| **Date** | 2026-04-13 |

## 1. Functional Coverage Summary

This crate implements a simple echo service (~117 lines) that swaps source and destination EIDs and returns the bundle. It has no assigned LLRs. Correctness is verified end-to-end by interop tests across all 7 DTN implementations.

## 2. Test Inventory

### Unit Tests

No unit tests are implemented — see [`PLAN-ECHO-01`](test_plan.md) §4 for rationale.

### End-to-End Verification

| Test | Location | Scope |
| :--- | :--- | :--- |
| Interop tests | `tests/interop/` | All 7 peer implementations (dtn7-rs, HDTN, DTNME, ud3tn, ION, ESA BP, NASA cFS) send bundles to Hardy's echo service and verify the response. See [`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md) |

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-ECHO-01`](test_plan.md). The echo service is a single-function crate verified entirely through integration testing.

| Scope | Tests | Status |
| :--- | :--- | :--- |
| EID swap logic | 7 (interop) | Verified end-to-end via `bpa-server` + interop tests |
| Unit tests | 0 | No dedicated tests |

## 4. Line Coverage

Line coverage is not available. The crate has no unit tests. End-to-end verification runs through the BPA pipeline and interop test infrastructure.

## 5. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Unit tests | No isolated tests for EID swap logic | Low | Single-function crate; swap correctness verified by every interop test |
| Edge cases | Null EID, anonymous source, multicast destination not tested in isolation | Low | Defence-in-depth; BPA validates bundles before service dispatch |

## 6. Conclusion

The echo service is verified end-to-end by interop tests with all 7 DTN implementations (registered by `bpa-server`). The BPA pipeline test `echo_round_trip` uses an inline echo implementation for self-containment, not this crate. No dedicated unit tests exist; the crate is ~117 lines with a single function (EID swap + return). Edge-case coverage (null EIDs, anonymous sources) is a low-severity gap suitable for Full Activity.
