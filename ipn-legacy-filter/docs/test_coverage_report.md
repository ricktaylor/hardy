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

| Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- |
| Dedicated unit tests (IPNF-01..06) | 7 | 7 | Complete |
| Filter via BPA pipeline | — | Implicit | Rewriting path exercised |
| **Total** | **7** | **7** | **100%** |

## 4. Line Coverage

```
cargo llvm-cov test --package hardy-ipn-legacy-filter --lcov --output-path lcov.info
lcov --summary lcov.info
```

```
  lines......: 98.0% (98 of 100 lines)
  functions..: 100.0% (20 of 20 functions)
```

Unit tests (7) exercise pattern matching, EID rewriting, no-op paths, and configuration. The 2 uncovered lines are in the `WriteFilter` trait wiring (async trait boilerplate).

## 5. Conclusion

7 unit tests cover all planned scenarios (100%): allocator×matching matrix for both allocator_id=0 and allocator_id!=0, no-op paths (no next-hop, DTN endpoints, non-matching patterns), and empty configuration. The filter is also exercised implicitly through the BPA pipeline and interop tests.
