# IPN Legacy Filter Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-ipn-legacy-filter` |
| **Standard** | — |
| **Test Plans** | [`PLAN-IPNF-01`](unit_test_plan.md) |
| **Date** | 2026-04-13 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

This crate supports the interoperability aspects of RFC 9758 IPN encoding (LLR 1.1.23, 1.1.24). Both LLRs are primarily verified by the bpv7 crate; this filter verifies the encoding conversion path.

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| **1.1.23** | 3-element IPN encoding (RFC 9758) | Pass (bpv7 + filter) | `test_alloc1_matching` — converts 3-element to 2-element | 1.2 |
| **1.1.24** | Legacy 2-element detection | Pass (bpv7 + filter) | `test_alloc1_matching` — output parses as `LegacyIpn`; `test_alloc0_matching` — idempotent for allocator_id=0 | 1.2 |

## 2. Test Inventory

### Unit Tests

7 test functions in `src/lib.rs`.

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `test_empty_config` | IPNF-06 | `Config::default()` → `new()` returns `None` |
| `test_no_next_hop` | IPNF-06b | `next_hop = None` → no rewrite |
| `test_dtn_no_rewrite` | IPNF-06c | DTN source + dest → no rewrite |
| `test_alloc0_non_matching` | IPNF-01 | allocator_id=0, non-matching next-hop → no rewrite |
| `test_alloc0_matching` | IPNF-02 | allocator_id=0, matching → rewrite idempotent (already legacy on wire) |
| `test_alloc1_non_matching` | IPNF-03 | allocator_id!=0, non-matching → no rewrite |
| `test_alloc1_matching` | IPNF-04 | allocator_id!=0, matching → bytes change, output parses as `LegacyIpn` |

### Integration Verification

| Test | Location | Scope |
| :--- | :--- | :--- |
| BPA pipeline tests | `bpa/tests/` | Filter registered as egress filter; exercises the rewriting path when bundles are forwarded to peers requiring legacy encoding |
| Interop tests | `tests/interop/` | Implicitly exercises the filter when communicating with implementations that use 2-element IPN encoding |

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-IPNF-01`](unit_test_plan.md).

| Scope | Tests | Status |
| :--- | :--- | :--- |
| Filter via BPA pipeline | Implicit | Rewriting path exercised |
| Dedicated unit tests | 0 | Not implemented |

## 4. Line Coverage

Line coverage is not available. The crate has no unit tests. The filter is exercised indirectly through the BPA pipeline.

## 5. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Pattern matching | EID pattern matching logic not independently tested | Medium | Core filter logic should be verifiable without full BPA stack |
| EID rewriting | 3-element to 2-element conversion not independently tested | Medium | Correctness depends on `hardy-bpv7` EID encoding |
| No-op paths | Bundles not matching the filter pattern should pass through unchanged | Low | Important for correctness but low risk |
| Configuration | `serde` config deserialization not tested | Low | Thin config struct |

Unit tests for the filter transform are recommended. The filter's pattern matching and EID rewriting logic should be independently verifiable without standing up the full BPA pipeline.

## 6. Conclusion

The IPN legacy filter is currently verified only through implicit BPA pipeline and interop testing. No dedicated unit tests exist; the crate is ~112 lines with pattern matching and EID rewriting logic. Unit tests covering the filter transform (pattern matching, 3-to-2 element rewriting, no-op passthrough) are recommended as a Medium-severity gap. The filter's correctness is critical for interoperability with legacy DTN implementations.
