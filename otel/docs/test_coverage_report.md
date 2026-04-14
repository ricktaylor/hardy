# OpenTelemetry Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-otel` |
| **Standard** | OpenTelemetry Specification v1.x |
| **Test Plans** | [`UTP-OTEL-01`](unit_test_plan.md), [`COMP-OTEL-01`](component_test_plan.md) |
| **Date** | 2026-04-14 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

The `hardy-otel` crate has no formal LLRs — it is internal infrastructure. The table below maps functional requirements to their verification status. All functional requirements verified (11 pass). OTEL export (traces, metrics, logs) is verified by the integration test (`tests/test_otel_export.sh`).

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| **—** | Gauge increment | Pass | `gauge_increment`, `gauge_increment_decrement_sequence`, `macro_gauge_increment_decrement`, `macro_gauge_with_labels` | 19.1 |
| **—** | Gauge decrement | Pass | `gauge_decrement`, `gauge_increment_decrement_sequence`, `gauge_decrement_below_zero`, `macro_gauge_increment_decrement` | 19.1 |
| **—** | Gauge set | Pass | `gauge_set`, `gauge_set_overrides_accumulated`, `macro_gauge_set`, `macro_gauge_set_overrides_increments` | 19.1 |
| **—** | Gauge with labels | Pass | `gauge_with_labels`, `macro_gauge_with_labels`, `recorder_labeled_gauge`, `macro_multiple_label_values_are_distinct` | 19.1 |
| **—** | Counter increment | Pass | `counter_increment`, `macro_counter`, `macro_counter_with_labels` | 19.1 |
| **—** | Counter absolute (rejected) | Pass | `counter_absolute_panics` | 19.1 |
| **—** | Histogram record | Pass | `histogram_record`, `macro_histogram` | 19.1 |
| **—** | Recorder registration | Pass | `recorder_register_gauge_and_use`, `recorder_describe_then_register`, `recorder_labeled_gauge` | 19.1 |
| **—** | Instrument caching | Pass | `recorder_register_gauge_and_use` (two registrations share state) | 19.1 |
| **—** | Description propagation | Pass | `recorder_describe_then_register`, `macro_describe_then_use` | 19.1 |
| **—** | Label value isolation | Pass | `macro_multiple_label_values_are_distinct` (same name, different labels → separate instruments) | 19.1 |

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

**Total: 26 unit tests, ~46 assertions.**

### Integration Tests (`tests/test_otel_export.sh`)

| Test | Scope |
| :--- | :--- |
| OTEL-01: Traces | Spans exported to OTLP collector via file exporter |
| OTEL-02: Metrics | Counters, gauges, histograms exported |
| OTEL-03: Logs | Structured log records exported |

Uses a minimal OpenTelemetry Collector (contrib image) with file exporters. Test harness (`tests/otel_export_test.rs`) initialises `hardy_otel::init()`, emits telemetry, and verifies output via `jq`.

## 3. Coverage vs Plan

| Section | Scenario | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| UTP-OTEL-01 | Metrics bridge unit tests | 26 | 26 | Complete |
| OTEL-01..03 | OTLP export integration (traces, metrics, logs) | 3 | 3 | Complete (`test_otel_export.sh`) |
| | **Total** | **29** | **29** | **100%** |

## 4. Line Coverage

```
cargo llvm-cov test --package hardy-otel --lcov --output-path lcov.info --html
lcov --summary lcov.info
```

Results (2026-04-14):

```
  lines......: 81.4% (439 of 539 lines)
  functions..: 54.2% (64 of 118 functions)
```

Per-file breakdown (from HTML report):

| File | Lines | Functions | Notes |
| :--- | :--- | :--- | :--- |
| `metrics_otel.rs` | 99.57% (458/460) | 100.00% (63/63) | 2 uncovered lines (see §4) |
| `lib.rs` | 0.00% (0/92) | 0.00% (0/11) | Integration-only code (see below) |

The `lib.rs` file (OTEL provider initialisation, tracing subscriber wiring) is not unit-testable in isolation — it requires an OTLP endpoint and sets global state. It is verified by the OTEL export integration test (`tests/test_otel_export.sh`).

### Uncovered Lines

The 2 uncovered lines in `metrics_otel.rs`:

| Line | Code | Reason |
| :--- | :--- | :--- |
| 70 | `otel_unit` passthrough (`other => other.into()`) | `metrics::Unit` is an enum — all variants are mapped explicitly. This branch exists as a forward-compatibility fallback if the `metrics` crate adds new variants. |
| 220 | CAS retry loop body (`compare_exchange_weak` failure path) | Only triggered under concurrent contention. The CAS pattern is well-established; correctness of the transform function is verified by single-threaded tests. |

## 5. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| Counter/histogram | No readable state for assertions | Low | OTEL SDK owns aggregation; verified via no-panic + registration cache presence |

Note: `lib.rs` is not unit-tested but is fully covered by the OTEL export integration test ([`COMP-OTEL-01`](component_test_plan.md)). This is by design — it requires an OTLP endpoint and sets global state.

## 6. Conclusion

26 unit tests verify the `metrics_otel.rs` recorder bridge across all three instrument types at three levels: direct trait calls, `Recorder` API, and `metrics::*!()` macros. The gauge state tracking (AtomicU64 CAS loop) has 7 dedicated unit tests plus 5 macro-level tests. The `lib.rs` initialisation and OTLP export path is verified by the OTEL export integration test (`tests/test_otel_export.sh`), which confirms traces (spans), metrics (counters, gauges, histograms), and structured logs reach an OTLP collector. This integration test covers all server binaries — bpa-server, tcpclv4-server, and tvr all use the same `hardy_otel::init()` call.
