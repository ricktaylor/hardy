# Test Plan: BPA Server Application

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Application Runtime & Observability |
| **Module** | `hardy-bpa-server` |
| **Requirements Ref** | [REQ-7](../../docs/requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [REQ-13](../../docs/requirements.md#req-13-performance), [REQ-14](../../docs/requirements.md#req-14-reliability), [REQ-15](../../docs/requirements.md#req-15-independent-component-packaging), [REQ-16](../../docs/requirements.md#req-16-kubernetes-packaging), [REQ-19](../../docs/requirements.md#req-19-a-well-featured-suite-of-management-and-monitoring-tools), [LLR 7.1.x](../../docs/requirements.md#314-local-disk-storage-parent-req-7), [LLR 19.x](../../docs/requirements.md#317-opentelemetry-parent-req-19) |
| **Standard Ref** | OpenTelemetry (OTLP) |
| **Test Suite ID** | PLAN-SERVER-01 |

## 1. Introduction

This document details the testing strategy for the `hardy-bpa-server` module. Unlike the core libraries, this module is the **deployable executable**. Its primary responsibility is to bootstrap the system, load configuration, initialize telemetry, and wire up the internal components (BPA, TCPCL, Storage).

**Scope:**

* **Configuration Management:** Merging of Config Files (TOML) with Environment Variables.

* **Process Lifecycle:** Clean startup and graceful shutdown (Signal Handling).

* **Observability (OTEL):** Verification that Traces, Metrics, and Logs are correctly emitted to an external collector.

* **gRPC Binding:** Ensuring API endpoints are exposed on the configured ports.

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by the unit tests in this plan:

| LLR ID | Description |
| :--- | :--- |
| **3.1.5** | Allow configuration of default session parameters (Keepalive, Segment Size). |
| **7.1.1** | Configurable location for Local Disk bundle storage. |
| **7.1.2** | Configurable maximum total size for Local Disk storage. |
| **7.2.2** | Configurable filesystem location for SQLite database. |

## 3. Unit Test Cases

### 3.1 Configuration Logic (LLR 3.1.5, 7.1.1, 7.1.2, 7.2.2)

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Default Load (CFG-01)** | Verify that an empty or missing config file results in a valid configuration object populated with safe defaults. | `src/config.rs` | Empty config file. | Configuration struct populated with safe defaults (e.g., localhost:4556). |
| **TOML Parsing (CFG-02)** | Verify that values from a valid `hardy.toml` file correctly override the defaults. | `src/config.rs` | Valid `hardy.toml` with overridden storage path. | `config.storage.path` matches file value. |
| **Env Override (CFG-03)** | Verify that environment variables take precedence over values from a config file. | `src/config.rs` | File: `port=4000`<br>Env: `HARDY_PORT=5000` | Effective config uses `5000` (Env wins). |
| **Validation (CFG-04)** | Verify that the parser rejects configuration with invalid values (e.g., out of range). | `src/config.rs` | Config with `keepalive=0` (Invalid). | Parser returns `ConfigError::InvalidValue`. |
| **No Config File (CFG-05)** | Verify that the server can start using only default values if no config file is specified. | `src/config.rs` | Run without `-c` arg. | Defaults loaded, no error about missing file. |
| **Path Resolution (CFG-06)** | Verify that relative paths in the config file are resolved correctly relative to a defined base. | `src/config.rs` | Config `storage.path = "./db"`. | Path resolved relative to CWD or Config File (Defined behavior). |

## 4. System Test Cases (Black-Box Execution)

*Scope: Running the compiled binary `target/debug/hardy-bpa-server`.*

### 4.1 Lifecycle & Signals

*Objective: Verify the application behaves like a good Cloud Native citizen.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **SYS-01** | **Startup Smoke Test** | 1. Run `./hardy-bpa-server`.<br>2. Check `netstat` for listening ports. | Process stays running.<br>TCP ports 4556 (TCPCL) and 50051 (gRPC) are LISTEN. |
| **SYS-02** | **Graceful Shutdown (SIGINT)** | 1. Start server.<br>2. Send `Ctrl+C` (SIGINT) to running process. | Logs show "Shutting down...".<br>Process exits with code 0.<br>Storage lockfiles (if any) are cleaned up. |
| **SYS-03** | **Configuration Error** | 1. Run with invalid config path (`-c /nonexistent`). | Process exits immediately with non-zero code.<br>Stderr prints readable error message. |
| **SYS-04** | **CLI Arguments** | 1. Run with `--help`.<br>2. Run with `--version`. | Prints usage info/version respectively and exits 0. |
| **SYS-05** | **Panic Propagation** | 1. Start server.<br>2. Trigger panic in sub-task (e.g., via debug API). | Process exits non-zero (Fail-fast). |

### 4.2 Observability & OpenTelemetry (REQ-19)

*Objective: Verify integration with `hardy-otel` and the OTLP exporter.*
*Harness: Run a local **Grafana LGTM** (Loki, Grafana, Tempo, Mimir) container to receive data.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **OTEL-01** | **Trace Emission** | 1. Configure `otel_endpoint = "http://localhost:4317"`.<br>2. Start Server.<br>3. Send a Bundle via `bping`.<br>4. Query Grafana Tempo for traces. | Trace exists for bundle transmission.<br>Trace contains spans: `bpa.receive`, `bpa.route`, `tcpcl.forward`. |
| **OTEL-02** | **Metric Export** | 1. Start Server.<br>2. Send 100 bundles via `bping`.<br>3. Query Prometheus/OTEL metrics endpoint. | Metric `dtn_bundles_processed_total` increases.<br>Metric `dtn_storage_bytes` reflects usage. |
| **OTEL-03** | **Structured Logging** | 1. Configure `log_format = "json"`.<br>2. Start Server.<br>3. Trigger an error (e.g., bad auth). | Stdout shows JSON formatted logs with `trace_id` and `span_id` correlated. |

### 4.3 Integration Verification

*Objective: Ensure sub-modules are wired correctly.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **INT-01** | **Storage Backend Loading** | 1. Configure `storage_type = "sqlite"`.<br>2. Start Server.<br>3. Check logs and filesystem. | Logs show "Initializing SQLite Storage".<br>`.db` file created on disk. |
| **INT-02** | **TCPCL Listener** | 1. Configure `tcpcl_port = 9999`.<br>2. Start Server.<br>3. Run `telnet localhost 9999`. | Logs show "TCPCL listening on 0.0.0.0:9999".<br>Telnet connection succeeds. |
| **INT-03** | **Management API** | 1. Configure `management_port = 50051`.<br>2. Start Server.<br>3. Run `grpcurl` against endpoint. | Logs show "Management Service listening".<br>gRPC client connects successfully. |
| **INT-04** | **Health Check** | 1. Start Server.<br>2. Query `/health` (HTTP) or gRPC Health service. | Returns `SERVING` / 200 OK. |

### 4.4 Performance & Scalability (REQ-13)

*Objective: Verify system compliance with critical performance metrics.*
*Harness: Dedicated performance environment (e.g., 2 nodes connected via 10GbE).*

| Test ID | Scenario | Procedure | Pass Criteria |
| ----- | ----- | ----- | ----- |
| **PERF-SYS-01** | **High Throughput (10Gbps)** | 1. Generate large bundles (10MB+).<br>2. Send via TCPCL over 10GbE link.<br>3. Measure goodput. | Throughput approaches 10Gbps (link saturation). |
| **PERF-SYS-02** | **Bundle Processing Rate** | 1. Generate small bundles (1KB).<br>2. Send burst of 100k bundles.<br>3. Measure processing time. | Rate > 1000 bundles/sec. |
| **PERF-SYS-03** | **Large Reassembly** | 1. Fragment a 4.1GB file into 10MB chunks.<br>2. Send fragments to server.<br>3. Verify reassembled payload. | 1. Reassembly succeeds.<br>2. MD5 checksum matches.<br>3. No OOM crash. |
| **PERF-SYS-04** | **Storage Scalability** | 1. Fill storage with 1 million small bundles (metadata stress).<br>2. Perform retrieval/deletion operations. | Operations remain O(1) or O(log n).<br>No significant latency degradation. |

### 4.5 Latency Profiling

*Objective: Verify latency distribution meets SLA requirements.*

| Test ID | Scenario | Procedure | Pass Criteria |
| :--- | :--- | :--- | :--- |
| **PERF-LAT-SYS-01** | **End-to-End Latency** | 1. Configure 3-node topology (A -> B -> C).<br>2. Send 10,000 bundles via `hardy-ping`.<br>3. Compute latency percentiles. | P50 < 10ms, P95 < 50ms, P99 < 100ms |

### 4.6 Stress and Soak Testing

*Objective: Verify system stability under sustained load and detect resource leaks.*

| Test ID | Scenario | Duration | Pass Criteria |
| :--- | :--- | :--- | :--- |
| **STRESS-01** | **Sustained Load** | 4 hours at 80% max throughput | No crashes. Memory usage stable (< 10% growth). Throughput variance < 5%. |
| **STRESS-02** | **Memory Leak Detection** | 24 hours at moderate load | RSS memory at T=24h within 5% of T=1h. No OOM events. |
| **STRESS-03** | **Handle Exhaustion** | Rapid connection open/close cycles (10k connections) | All connections handled. File descriptor count returns to baseline. |
| **STRESS-04** | **Storage Fill/Drain Cycle** | Fill storage to 95%, drain, repeat 10 times | No storage corruption. Performance consistent across cycles. |

*Tooling: Use `valgrind --tool=massif` or `heaptrack` for memory profiling. Monitor `/proc/{pid}/fd` for handle exhaustion.*

### 4.7 Network Degradation Testing

*Objective: Verify graceful degradation under adverse network conditions.*

| Test ID | Network Condition | Procedure | Pass Criteria |
| :--- | :--- | :--- | :--- |
| **NET-01** | **Packet Loss (5%)** | Apply `tc netem loss 5%` between nodes. Run throughput test. | Throughput > 50% of baseline. No data corruption. |
| **NET-02** | **High Latency (500ms)** | Apply `tc netem delay 500ms`. Run throughput test. | Bundles delivered successfully. Keepalive timers adjusted. |
| **NET-03** | **Jitter (100ms +/- 50ms)** | Apply `tc netem delay 100ms 50ms`. Transfer 1000 bundles. | All bundles delivered. Reassembly succeeds. |
| **NET-04** | **Bandwidth Limit (1Mbps)** | Apply `tc tbf rate 1mbit`. Send 100MB of bundles. | Transfer completes. TCPCL backpressure functions correctly. |
| **NET-05** | **Intermittent Connectivity** | Alternate 30s connected / 30s disconnected. | All bundles eventually delivered. No duplicate deliveries. |

*Tooling: Use Linux Traffic Control (`tc`) with `netem` for network simulation.*

### 4.8 Resource Utilization Targets

*Objective: Define and verify resource consumption bounds.*

| Test ID | Scenario | Procedure | Pass Criteria |
| :--- | :--- | :--- | :--- |
| **RES-01** | **Idle Resource Usage** | Start server with no traffic. Wait 5 minutes. Measure CPU/memory. | CPU < 1%. RSS < 100MB. |
| **RES-02** | **Memory Scaling** | Store 100k bundles, measure memory. Store 500k bundles, measure memory. | Memory scales linearly or sub-linearly. |
| **RES-03** | **CPU Efficiency** | Measure bundles/sec at 50% CPU, then at 100% CPU. | Linear scaling (no lock contention). |

*Tooling: Use Prometheus metrics or `/proc/{pid}/stat` for measurement.*

### 4.9 Packaging & Deployment (REQ-15, REQ-16)

*Objective: Verify the build artifacts (Docker Image, Helm Chart) are valid and secure.*

| Test ID | Scenario | Procedure | Pass Criteria |
| ----- | ----- | ----- | ----- |
| **PKG-OCI-01** | **Image Structure** | Inspect image layers and metadata. | Base image is `distroless` or minimal.<br>Entrypoint is `/usr/local/bin/hardy-bpa-server`. |
| **PKG-OCI-02** | **Security Context** | Run container and check user. | Process runs as non-root (UID != 0). |
| **PKG-OCI-03** | **Vulnerability Scan** | Run `trivy image hardy-bpa-server`. | No Critical/High CVEs found. |
| **PKG-HELM-01** | **Chart Lint** | Run `helm lint charts/hardy`. | No errors or warnings. |
| **PKG-HELM-02** | **Template Render** | Run `helm template charts/hardy`. | Generates valid YAML.<br>ConfigMap contains `hardy.toml`. |
| **PKG-HELM-03** | **Install Cycle** | `helm install` -> `helm test` -> `helm uninstall`. | Pods reach `Running`.<br>Tests pass.<br>Resources cleaned up. |

## 5. Execution Strategy

To automate **Section 4**, use a simple Python or Bash harness that:

1. Spins up `grafana/otel-lgtm` via Docker.
2. Starts `hardy-bpa-server` in the background.
3. Runs `hardy-bping` to generate traffic.
4. Queries the Tempo API to assert that traces exist.
