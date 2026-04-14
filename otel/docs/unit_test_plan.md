# Unit Test Plan: OpenTelemetry Recorder

| Document Info | Details |
| ----- | ----- |
| **Functional Area** | Observability / Metrics Bridge |
| **Module** | `hardy-otel` |
| **Requirements Ref** | [REQ-19](../../docs/requirements.md#req-19-management-and-monitoring-tools) |
| **Standard Ref** | OpenTelemetry Metrics SDK 0.31, `metrics` crate 0.24 |
| **Test Suite ID** | UTP-OTEL-01 |
| **Version** | 1.0 |

## 1. Introduction

This document details the unit testing strategy for the `hardy-otel` module. This module bridges the `metrics` crate facade to OpenTelemetry instruments, allowing BPA and other Hardy components to emit OTEL-compliant metrics using the idiomatic `metrics::counter!()` / `metrics::gauge!()` / `metrics::histogram!()` macros.

**Scope:**

* **Gauge state tracking:** Verifying the `AtomicU64`-based CAS loop correctly tracks gauge values across `increment`, `decrement`, and `set` operations.

* **Counter forwarding:** Verifying counters forward to OTEL without panic, and that `absolute()` is correctly rejected.

* **Histogram forwarding:** Verifying histogram recording forwards to OTEL without panic.

* **Recorder registration:** Verifying instrument caching, description propagation, and label handling.

* **Macro integration:** Verifying the full path from `metrics::*!()` macros through `with_local_recorder` to the OpenTelemetry bridge.

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by this plan:

| Requirement | Description | Test Coverage |
| ----- | ----- | ----- |
| **REQ-19** | Management and monitoring tools | OTEL metrics bridge correctness |

## 3. Unit Test Cases

All tests are located in `otel/src/metrics_otel.rs`.

### 3.1 Gauge State Tracking (InnerGauge)

*Objective: Verify the AtomicU64-based CAS loop correctly maintains gauge state for increment, decrement, and set operations.*

| Test Scenario | Test Function | Operation | Expected Value |
| ----- | ----- | ----- | ----- |
| **Set absolute value** | `gauge_set` | `set(42.0)` | 42.0 |
| **Accumulate increments** | `gauge_increment` | `increment(1.0)`, `increment(2.5)` | 3.5 |
| **Decrement from set value** | `gauge_decrement` | `set(10.0)`, `decrement(3.0)` | 7.0 |
| **Mixed increment/decrement** | `gauge_increment_decrement_sequence` | 3x `increment(1.0)`, 1x `decrement(1.0)` | 2.0 |
| **Set overrides accumulated** | `gauge_set_overrides_accumulated` | `increment(5.0)`, `set(100.0)` | 100.0 |
| **Decrement below zero** | `gauge_decrement_below_zero` | `decrement(1.0)` from 0.0 | -1.0 |
| **Gauge with labels** | `gauge_with_labels` | `increment(1.0)` on labeled gauge | 1.0 |

### 3.2 Counter Forwarding (InnerCounter)

*Objective: Verify counter operations forward correctly and invalid operations are rejected.*

| Test Scenario | Test Function | Operation | Expected Output |
| ----- | ----- | ----- | ----- |
| **Increment (no panic)** | `counter_increment` | `increment(1)`, `increment(100)` | No panic |
| **Absolute rejected** | `counter_absolute_panics` | `absolute(42)` | **Panic** (`absolute() is not supported`) |

### 3.3 Histogram Forwarding (InnerHistogram)

*Objective: Verify histogram recording forwards correctly.*

| Test Scenario | Test Function | Operation | Expected Output |
| ----- | ----- | ----- | ----- |
| **Record (no panic)** | `histogram_record` | `record(1.5)`, `record(100.0)` | No panic |

### 3.4 Recorder Registration & Caching

*Objective: Verify the `OpenTelemetryRecorder` correctly registers, caches, and describes instruments.*

| Test Scenario | Test Function | Verification |
| ----- | ----- | ----- |
| **Register and use gauge** | `recorder_register_gauge_and_use` | Increment/decrement via recorder; verify cached value = 7.0 |
| **Instrument caching** | `recorder_register_gauge_and_use` | Second `register_gauge()` returns same `InnerGauge` (shared state) |
| **Describe then register** | `recorder_describe_then_register` | Descriptions stored before registration; all 3 types register without panic |
| **Labeled gauge registration** | `recorder_labeled_gauge` | Labels propagate to `InnerGauge.labels`; key/value verified |

### 3.5 Unit String Mapping

*Objective: Verify that `metrics::Unit` values are correctly mapped to OTEL/UCUM unit strings.*

| Test Scenario | Test Function | Verification |
| ----- | ----- | ----- |
| **All standard units** | `otel_unit_mapping` | All 17 `metrics::Unit` variants map to correct UCUM codes (e.g. Count→"1", Seconds→"s", Bytes→"By") |

### 3.6 Metrics Macro Integration

*Objective: Verify the full path from `metrics::*!()` macros through `with_local_recorder` to the OpenTelemetry bridge. These tests exercise the exact API that BPA code uses.*

| Test Scenario | Test Function | Macro | Verification |
| ----- | ----- | ----- | ----- |
| **Gauge inc/dec via macro** | `macro_gauge_increment_decrement` | `metrics::gauge!()` | 3x inc - 1x dec = 2.0 |
| **Gauge set via macro** | `macro_gauge_set` | `metrics::gauge!()` | `set(42.0)` → 42.0 |
| **Set overrides inc via macro** | `macro_gauge_set_overrides_increments` | `metrics::gauge!()` | `inc(10.0)`, `set(0.0)` → 0.0 |
| **Labeled gauge via macro** | `macro_gauge_with_labels` | `metrics::gauge!(name, "k" => "v")` | 5.0 - 2.0 = 3.0 |
| **Counter via macro** | `macro_counter` | `metrics::counter!()` | Registered in cache, no panic |
| **Labeled counter via macro** | `macro_counter_with_labels` | `metrics::counter!(name, "k" => "v")` | Registered in cache, no panic |
| **Histogram via macro** | `macro_histogram` | `metrics::histogram!()` | Registered in cache, no panic |
| **Labeled histogram via macro** | `macro_histogram_with_labels` | `metrics::histogram!(name, "k" => "v")` | Labels propagate; key/value verified |
| **Describe then use (all types)** | `macro_describe_then_use` | `describe_*!()` + `*!()` | Descriptions stored; gauge value correct |
| **Use without describe** | `macro_use_without_describe` | `counter!()`, `gauge!()`, `histogram!()` | No prior `describe_*!()`; instruments register and function correctly |
| **Distinct label values** | `macro_multiple_label_values_are_distinct` | `metrics::gauge!(name, "r" => "a"/"b")` | Separate instruments: a=1.0, b=10.0 |

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-otel`

* **Pass Criteria:** All 26 tests listed above must return `ok`.

* **Coverage Target:** 100% line coverage for `src/metrics_otel.rs` (all `GaugeFn`, `CounterFn`, `HistogramFn` trait methods and `Recorder` registration methods exercised).
