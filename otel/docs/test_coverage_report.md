# OpenTelemetry Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-otel` |
| **Standard** | OpenTelemetry Specification v1.x |
| **Test Plan** | [`UTP-OTEL-01`](unit_test_plan.md) |
| **Date** | 2026-04-02 |

## 1. Functional Coverage Summary

All `metrics::Recorder` trait methods and all `CounterFn`/`GaugeFn`/`HistogramFn` trait implementations are exercised.

| Feature | Status | Tests |
| :--- | :--- | :--- |
| **Gauge increment** | Verified | `gauge_increment`, `gauge_increment_decrement_sequence`, `macro_gauge_increment_decrement`, `macro_gauge_with_labels` |
| **Gauge decrement** | Verified | `gauge_decrement`, `gauge_increment_decrement_sequence`, `gauge_decrement_below_zero`, `macro_gauge_increment_decrement` |
| **Gauge set** | Verified | `gauge_set`, `gauge_set_overrides_accumulated`, `macro_gauge_set`, `macro_gauge_set_overrides_increments` |
| **Gauge with labels** | Verified | `gauge_with_labels`, `macro_gauge_with_labels`, `recorder_labeled_gauge`, `macro_multiple_label_values_are_distinct` |
| **Counter increment** | Verified | `counter_increment`, `macro_counter`, `macro_counter_with_labels` |
| **Counter absolute (rejected)** | Verified | `counter_absolute_panics` |
| **Histogram record** | Verified | `histogram_record`, `macro_histogram` |
| **Recorder registration** | Verified | `recorder_register_gauge_and_use`, `recorder_describe_then_register`, `recorder_labeled_gauge` |
| **Instrument caching** | Verified | `recorder_register_gauge_and_use` (two registrations share state) |
| **Description propagation** | Verified | `recorder_describe_then_register`, `macro_describe_then_use` |
| **Label value isolation** | Verified | `macro_multiple_label_values_are_distinct` (same name, different labels → separate instruments) |

## 2. Test Inventory

| Test Function | File | Assertions | Scope |
| :--- | :--- | :--- | :--- |
| `gauge_set` | `metrics_otel.rs` | 1 | `GaugeFn::set()` absolute value |
| `gauge_increment` | `metrics_otel.rs` | 1 | `GaugeFn::increment()` accumulation |
| `gauge_decrement` | `metrics_otel.rs` | 1 | `GaugeFn::decrement()` from set value |
| `gauge_increment_decrement_sequence` | `metrics_otel.rs` | 1 | Mixed inc/dec sequence |
| `gauge_set_overrides_accumulated` | `metrics_otel.rs` | 1 | `set()` replaces accumulated value |
| `gauge_decrement_below_zero` | `metrics_otel.rs` | 1 | Negative gauge value |
| `gauge_with_labels` | `metrics_otel.rs` | 1 | Labeled `InnerGauge` state tracking |
| `counter_increment` | `metrics_otel.rs` | 0 | `CounterFn::increment()` no-panic |
| `counter_absolute_panics` | `metrics_otel.rs` | 0 | `CounterFn::absolute()` rejected |
| `histogram_record` | `metrics_otel.rs` | 0 | `HistogramFn::record()` no-panic |
| `recorder_register_gauge_and_use` | `metrics_otel.rs` | 1 | Registration, caching, state via `Recorder` API |
| `recorder_describe_then_register` | `metrics_otel.rs` | 0 | Description storage, all 3 types no-panic |
| `recorder_labeled_gauge` | `metrics_otel.rs` | 4 | Label propagation through `Recorder` API |
| `macro_gauge_increment_decrement` | `metrics_otel.rs` | 1 | `metrics::gauge!()` macro inc/dec |
| `macro_gauge_set` | `metrics_otel.rs` | 1 | `metrics::gauge!()` macro set |
| `macro_gauge_set_overrides_increments` | `metrics_otel.rs` | 1 | `metrics::gauge!()` set overrides |
| `macro_gauge_with_labels` | `metrics_otel.rs` | 1 | `metrics::gauge!()` with labels |
| `macro_counter` | `metrics_otel.rs` | 1 | `metrics::counter!()` macro |
| `macro_counter_with_labels` | `metrics_otel.rs` | 1 | `metrics::counter!()` with labels |
| `macro_histogram` | `metrics_otel.rs` | 1 | `metrics::histogram!()` macro |
| `macro_histogram_with_labels` | `metrics_otel.rs` | 3 | `metrics::histogram!()` with labels |
| `macro_describe_then_use` | `metrics_otel.rs` | 4 | `describe_*!()` then `*!()` all 3 types |
| `macro_use_without_describe` | `metrics_otel.rs` | 1 | All 3 types used without prior `describe_*!()` |
| `otel_unit_mapping` | `metrics_otel.rs` | 17 | All `metrics::Unit` variants → UCUM codes |
| `macro_multiple_label_values_are_distinct` | `metrics_otel.rs` | 2 | Distinct label values → separate instruments |

**Total: 26 tests, ~46 assertions.**

## 3. Line Coverage

```
cargo llvm-cov test --package hardy-otel --lcov --output-path lcov.info --html
lcov --summary lcov.info
```

Results (2026-04-02):

```
  lines......: 83.3% (439 of 527 lines)
  functions..: 85.3% (64 of 75 functions)
```

Per-file breakdown (from HTML report):

| File | Lines | Functions | Notes |
| :--- | :--- | :--- | :--- |
| `metrics_otel.rs` | 99.57% (458/460) | 100.00% (63/63) | 2 uncovered lines (see §4) |
| `lib.rs` | 0.00% (0/92) | 0.00% (0/11) | Integration-only code (see below) |

The `lib.rs` file (OTEL provider initialisation, tracing subscriber wiring) is not unit-testable in isolation — it requires an OTLP endpoint and sets global state. It is verified at the system level through `bpa-server` and `tcpclv4-server` integration tests.

## 4. Uncovered Lines

The 2 uncovered lines in `metrics_otel.rs`:

| Line | Code | Reason |
| :--- | :--- | :--- |
| 70 | `otel_unit` passthrough (`other => other.into()`) | `metrics::Unit` is an enum — all variants are mapped explicitly. This branch exists as a forward-compatibility fallback if the `metrics` crate adds new variants. |
| 220 | CAS retry loop body (`compare_exchange_weak` failure path) | Only triggered under concurrent contention. The CAS pattern is well-established; correctness of the transform function is verified by single-threaded tests. |

## 5. Known Gaps

| Gap | Impact | Mitigation |
| :--- | :--- | :--- |
| `lib.rs` not unit-tested | Low — integration-only code | Covered by system-level test plans ([`PLAN-SERVER-01`](../../bpa-server/docs/test_plan.md)) |
| Counter/histogram have no readable state | Low — OTEL SDK owns the aggregation | Verified via no-panic + registration cache presence; OTEL SDK has its own test suite |

## 5. Conclusion

The `metrics_otel.rs` recorder bridge has comprehensive test coverage across all three instrument types at three levels: direct trait calls, `Recorder` API, and `metrics::*!()` macros. The gauge state tracking (AtomicU64 CAS loop) — the most complex logic in the crate — has 7 dedicated unit tests plus 5 macro-level tests covering set, increment, decrement, label isolation, and edge cases (negative values, set-overrides-accumulation). The `lib.rs` initialisation code is an integration concern appropriately tested at the system level.
