# Component Test Plan: OpenTelemetry Export

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Observability (OTLP Export) |
| **Module** | `hardy-otel` |
| **Requirements Ref** | [REQ-19](../../docs/requirements.md#req-19-a-well-featured-suite-of-management-and-monitoring-tools) |
| **Test Suite ID** | `COMP-OTEL-01` |
| **Version** | 1.0 |

## 1. Introduction

This document details the component-level testing strategy for `hardy-otel`'s OTLP export functionality. These tests verify that `hardy_otel::init()` correctly exports traces, metrics, and logs to an OTLP-compatible collector — covering the `lib.rs` initialisation code that is not exercised by the unit tests in [`UTP-OTEL-01`](unit_test_plan.md).

**Scope:**

* Trace export — spans reach the collector via gRPC (tonic).
* Metric export — counters, gauges, and histograms reach the collector.
* Log export — structured log records reach the collector.

**Delegation:**

All server binaries (bpa-server, tcpclv4-server, tvr) use the same `hardy_otel::init()` call. Testing it once at the library level covers all binaries. Server-specific test plans reference this plan for OTEL coverage.

## 2. Testing Strategy

The test uses a real OpenTelemetry Collector (`otel/opentelemetry-collector-contrib`) with file exporters, and a Rust test harness that:

1. Calls `hardy_otel::init()` to initialise all three providers (tracer, meter, logger).
2. Emits traces (spans via `tracing`), metrics (via `metrics` crate), and logs (via `tracing`).
3. Calls `force_flush()` to ensure all data reaches the collector.
4. Drops the `OtelGuard` to shut down providers cleanly.

The shell script (`tests/test_otel_export.sh`) orchestrates the collector lifecycle and verifies the output files contain data using `jq`.

**Prerequisites:** Docker, jq, cargo.

## 3. Test Scenarios

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **OTEL-01** | **Trace Export** | 1. Emit spans via `tracing::info_span!`.<br>2. Flush and check `traces.jsonl`. | File contains spans with `resourceSpans`. |
| **OTEL-02** | **Metric Export** | 1. Emit counter, gauge, histogram via `metrics::*!()`.<br>2. Flush and check `metrics.jsonl`. | File contains metrics with `resourceMetrics`. |
| **OTEL-03** | **Log Export** | 1. Emit `tracing::info!` and `tracing::warn!` messages.<br>2. Flush and check `logs.jsonl`. | File contains log records with `resourceLogs`. |

## 4. Test Infrastructure

### Rust Harness (`tests/otel_export_test.rs`)

* `#[tokio::test(flavor = "multi_thread")]` — tonic needs background workers for HTTP/2.
* `#[ignore]` — requires an OTLP collector; run via shell script, not `cargo test`.
* 1-second yield after telemetry emission to let tonic establish gRPC connections.
* Explicit `force_flush()` before drop — in `#[tokio::test]` the runtime tears down workers before `drop()` runs, so the flush in `OtelGuard::drop()` may not complete for metrics/logs.

### Shell Script (`tests/test_otel_export.sh`)

* Starts `otel/opentelemetry-collector-contrib` with file exporters (traces, metrics, logs → separate JSONL files).
* Port-published (`-p 4317:4317`), world-writable output directory.
* Runs `cargo test` with `OTEL_EXPORTER_OTLP_ENDPOINT` set.
* Checks each output file with `jq` for non-empty arrays.

### Collector Config

Minimal config with OTLP gRPC receiver and three file exporters — no processing, no sampling.

## 5. Execution

```bash
./otel/tests/test_otel_export.sh
```

## 6. Key Design Decisions

### Library-Level Testing

OTEL export is tested at the `hardy-otel` crate level rather than in each server binary. All binaries call the same `hardy_otel::init()` with different `pkg_name`/`pkg_ver` values — the export mechanism is identical.

### `force_flush()` API

Added `OtelGuard::force_flush()` to support short-lived processes and test harnesses where the periodic metric export interval (60s) hasn't fired. Also called in `OtelGuard::drop()` as a safety net for server binaries.

### Multi-Thread Runtime Requirement

The tonic gRPC client requires background workers to establish HTTP/2 connections. Single-threaded tokio runtimes prevent the connection from establishing during `force_flush()`, causing timeouts.
