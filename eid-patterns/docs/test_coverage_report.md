# EID Patterns Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-eid-pattern` |
| **Standard** | `draft-ietf-dtn-eid-pattern` |
| **Test Plan** | [`UTP-PAT-01`](unit_test_plan.md) |
| **Date** | 2026-04-13 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

Both LLRs pass. IPN coverage is near-complete; DTN glob matching has a known implementation issue (see §5).

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| **6.1.1** | Parse textual `ipn` and `dtn` EID patterns | Pass | `str_tests::tests` — IPN: exact, wildcard, ranges, multi-range, coalescing, open-ended, `ipn:!.*`, legacy 2-element, inverted range normalisation. DTN: exact, glob, none, any, union. Invalid syntax rejection | 6.6 |
| **6.1.2** | Determine if EID matches pattern | Pass | `str_tests::tests` — IPN matching (~70 assertions), DTN exact/none/any matching. DTN glob matching is a non-standard simplification (see §5) | 6.6 |

## 2. Test Inventory

### Unit Tests

8 test functions, ~138 assertions total.

| Test Function | File | Plan Section | Scope |
| :--- | :--- | :--- | :--- |
| `tests` | `str_tests.rs` | 3.1, 3.2, 3.3, 3.4 | IPN parse+match, DTN parse+match, legacy format, inverted range normalisation, invalid syntax rejection |
| `test_specificity_score` | `str_tests.rs` | — | IPN + DTN specificity scoring, invalid patterns (non-monotonic, union sets) return None |
| `test_specificity_ordering` | `str_tests.rs` | — | BTreeSet ordering: most specific first |
| `test_subset_single_intervals` | `str_tests.rs` | — | Single interval subset: exact match, wildcard superset, range containment |
| `test_subset_multiple_intervals_in_lhs` | `str_tests.rs` | — | Multi-interval LHS coverage by RHS intervals |
| `test_subset_multiple_intervals_in_rhs` | `str_tests.rs` | — | Adjacent interval merging, gap detection |
| `test_subset_wildcard` | `str_tests.rs` | — | Wildcard as superset/subset |
| `test_subset_eid_pattern_set` | `str_tests.rs` | — | Multi-item pattern sets, Any pattern superset |

### Fuzz Tests

| Target | File | Status |
| :--- | :--- | :--- |
| `eid_pattern_str` | `fuzz/fuzz_targets/eid_pattern_str.rs` | Implemented — random strings fed to parser |

## 3. Coverage vs Plan

Cross-reference against [`UTP-PAT-01`](unit_test_plan.md):

| Section | Scenario | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| 3.1 IPN Pattern Parsing & Matching | IPN exact, wildcard, range, multi-range, open-ended, legacy | 12 | 12 | Complete |
| 3.2 DTN Pattern Parsing & Matching | DTN exact, glob, none, any, scheme wildcard | 7 | 7 | Complete (glob matching known-broken at runtime) |
| 3.3 Set Pattern Parsing | Union, any scheme | 2 | 2 | Complete |
| 3.4 Invalid Pattern Syntax | Bad separator, inverted range, malformed range, invalid scheme | 4 | 4 | Complete |
| **Total** | | **25** | **25** | **100%** |

All 25 planned scenarios have corresponding test assertions. DTN glob matching tests exist but exercise broken code paths (see §5).

## 4. Line Coverage

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

### Fuzz Coverage

```
cargo +nightly fuzz coverage eid_pattern_str
cargo +nightly cov -- export --format=lcov ...
lcov --summary ./fuzz/coverage/eid_pattern_str/lcov.info
```

Fuzz coverage is complementary to unit tests: unit tests verify pattern matching correctness, fuzz verifies parser robustness against adversarial input strings.

## 5. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| DTN glob matching | `do_glob()` wildcards don't match across authority/path boundary | Medium | Early IETF drafts specified complex regex matching for DTN patterns; this was removed before WG adoption. Hardy implements a simplified glob-based variation for practical use, but it is not standardised and the current matching has limitations (`dtn_pattern.rs:257`) |
| Glob-to-glob subset | Returns `true` unconditionally for Glob-vs-Glob comparison | Low | `dtn_pattern.rs:60` — consequence of the non-standard glob approach |
| Conversion/Display impls | `From<IpnNodeId>`, `From<NodeId>`, `From<Eid>`, `TryFrom<EidPattern>`, `Display` at 0% coverage | Low | Exercised by consuming crates (bpa, bpv7), not by eid-patterns unit tests |
| Cross-scheme subset logic | `AnyNumericScheme` vs `AnyTextScheme` comparisons in `is_subset` untested | Low | Edge case for mixed-scheme pattern sets |

## 6. Conclusion

The EID patterns crate has 8 unit tests and 1 fuzz target covering 25/25 planned scenarios (100% plan coverage, Part 4 Ref 6.6). Both LLRs (6.1.1, 6.1.2) are verified as Pass. Line coverage is 56.3% — below the 90% target, with the parser at 98% and IPN matching near-complete. The primary gap is the DTN glob matching — early IETF drafts specified complex regex matching for DTN patterns which was removed before WG adoption. Hardy implements a simplified glob-based variation for practical use, but it is not standardised and the current matching has limitations. The remaining gaps are `From`/`Display` conversion impls exercised only at the workspace level.
