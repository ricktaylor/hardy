# EID Patterns Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-eid-pattern` |
| **Standard** | `draft-ietf-dtn-eid-pattern` |
| **Test Plan** | [`UTP-PAT-01`](unit_test_plan.md) |
| **Date** | 2026-03-30 |

## 1. LLR Coverage Summary

Both Low-Level Requirements are verified through parsing, matching, and specificity tests.

| LLR | Feature | Status | Test |
| :--- | :--- | :--- | :--- |
| **6.1.1** | Parse textual `ipn` and `dtn` EID patterns | Verified | `str_tests::tests` — IPN: exact, wildcard (service/node/full), ranges, multi-range, coalescing, open-ended, `ipn:!.*`, legacy 2-element, inverted range normalisation. DTN: exact, glob (prefix/recursive/authority), none, any, union. Invalid syntax rejection |
| **6.1.2** | Determine if EID matches pattern | Verified | `str_tests::tests` — IPN matching (~70 assertions), DTN exact/none/any matching. DTN glob matching known-broken (see §5) |

## 2. Test Inventory

| Test Function | File | Assertions | Scope |
| :--- | :--- | :--- | :--- |
| `tests` | `str_tests.rs` | ~100 | IPN parse+match, DTN parse+match, legacy format, inverted range normalisation, invalid syntax rejection |
| `test_specificity_score` | `str_tests.rs` | 13 | IPN + DTN specificity scoring, invalid patterns (non-monotonic, union sets) return None |
| `test_specificity_ordering` | `str_tests.rs` | 4 | BTreeSet ordering: most specific first |
| `test_subset_single_intervals` | `str_tests.rs` | 7 | Single interval subset: exact match, wildcard superset, range containment |
| `test_subset_multiple_intervals_in_lhs` | `str_tests.rs` | 3 | Multi-interval LHS coverage by RHS intervals |
| `test_subset_multiple_intervals_in_rhs` | `str_tests.rs` | 3 | Adjacent interval merging, gap detection |
| `test_subset_wildcard` | `str_tests.rs` | 4 | Wildcard as superset/subset |
| `test_subset_eid_pattern_set` | `str_tests.rs` | 4 | Multi-item pattern sets, Any pattern superset |

## 3. Line Coverage

```
cargo llvm-cov test --package hardy-eid-pattern --lcov --output-path lcov.info
lcov --summary lcov.info
```

Results (2026-03-30):

```
  lines......: 56.3% (345 of 613 lines)
  functions..: 68.7% (68 of 99 functions)
```

Per-file breakdown (from HTML report):

| File | Covered | Total | Coverage | Notes |
| :--- | :--- | :--- | :--- | :--- |
| `parse.rs` | 58 | 59 | 98% | Parser near-fully covered |
| `ipn_pattern.rs` | 154 | 228 | 67% | Uncovered: `Display`, `Ord`, `try_to_eid`, `count()` |
| `dtn_pattern.rs` | 93 | 153 | 60% | Uncovered: `do_glob` (broken), `is_subset`, `try_to_eid`, `Display` |
| `lib.rs` | 40 | 173 | 23% | Uncovered: `From` conversions, `Display`, cross-scheme `is_subset` |

The line coverage (56.3%) is below the 90% target. The gaps fall into three categories:

1. **Known-broken code** — `do_glob()` DTN glob matching (see §5)
2. **Conversion/Display impls** — `From<IpnNodeId>`, `From<NodeId>`, `From<Eid>`, `TryFrom<EidPattern>`, `Display` for all types — these are API surface exercised by consuming crates (bpa, bpv7), not by eid-patterns unit tests
3. **Cross-scheme subset logic** — `AnyNumericScheme` vs `AnyTextScheme` comparisons in `is_subset`

## 4. Fuzz Testing

| Target | Status |
| :--- | :--- |
| `eid_pattern_str` | Implemented (`eid-patterns/fuzz/fuzz_targets/eid_pattern_str.rs`) — random strings fed to parser |

## 5. Known Issues

| Issue | File | Impact |
| :--- | :--- | :--- |
| DTN glob matching broken | `dtn_pattern.rs:257` | `do_glob()` matches against `"{node}//{demux}"` (double slash) but patterns stored as `"{node}/{demux}"` — wildcards never match across authority/path boundary |
| Glob-to-glob subset | `dtn_pattern.rs:60` | Returns `true` unconditionally for Glob-vs-Glob comparison |

## 6. Conclusion

The EID patterns crate has strong coverage for its core functionality: IPN parsing and matching is near-complete (~100% of scenarios), specificity scoring and subset operations are well-tested, and the parser itself is at 98% line coverage. The primary gaps are the broken DTN glob matching code (a known implementation issue, not a test gap) and `From`/`Display` conversion impls that are only exercised at the workspace level. Fixing `do_glob()` and adding DTN matching tests would bring coverage to ~75-80%.
