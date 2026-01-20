# Test Plan: BPA Server Application

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Application Runtime & Observability |
| **Module** | `hardy-bpa-server` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-7, REQ-13, REQ-14, REQ-15, REQ-16, REQ-19), `DTN-LLR_v1.1` (Section 4.3, 7.1) |
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

The following requirements from **DTN-LLR_v1.1** are verified by the unit tests in this plan:

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

| Test ID | Procedure | Expected Result |
 | ----- | ----- | ----- |
| **SYS-01** | **Startup Smoke Test** | 1. Run `./hardy-bpa-server`.<br>2. Check `netstat`. | Process stays running.<br>TCP ports 4556 (TCPCL) and 50051 (gRPC) are LISTEN. |
| **SYS-02** | **Graceful Shutdown (SIGINT)** | 1. Send `Ctrl+C` (SIGINT) to running process. | Logs show "Shutting down...".<br>Process exits with code 0.<br>Storage lockfiles (if any) are cleaned up. |
| **SYS-03** | **Configuration Error** | 1. Run with invalid config path. | Process exits immediately with non-zero code.<br>Stderr prints readable error message. |
| **SYS-04** | **CLI Arguments** | 1. Run with `--help` and `--version`. | Prints usage info/version and exits 0. |
| **SYS-05** | **Panic Propagation** | 1. Trigger panic in sub-task (e.g. via debug API). | Process exits non-zero (Fail-fast). |

### 4.2 Observability & OpenTelemetry (REQ-19)

*Objective: Verify integration with `hardy-otel` and the OTLP exporter.*
*Harness: Run a local **Grafana LGTM** (Loki, Grafana, Tempo, Mimir) container to receive data.*

| Test ID | Procedure | Expected Result |
 | ----- | ----- | ----- |
| **OTEL-01** | **Trace Emission** | 1. Configure `otel_endpoint = "http://localhost:4317"`.<br>2. Start Server.<br>3. Send a Bundle via `bping`. | Grafana (Tempo) shows a Trace for the bundle transmission.<br>Trace contains spans: `bpa.receive`, `bpa.route`, `tcpcl.forward`. |
| **OTEL-02** | **Metric Export** | 1. Run Server under load (100 bundles).<br>2. Check Prometheus/OTEL metrics endpoint. | Metric `dtn_bundles_processed_total` increases.<br>Metric `dtn_storage_bytes` reflects usage. |
| **OTEL-03** | **Structured Logging** | 1. Configure `log_format = "json"`.<br>2. Trigger an error (e.g., bad auth). | Stdout shows JSON formatted logs with `trace_id` and `span_id` correlated. |

### 4.3 Integration Verification

*Objective: Ensure sub-modules are wired correctly.*

| Test ID | Procedure | Expected Result |
 | ----- | ----- | ----- |
| **INT-01** | **Storage Backend Loading** | 1. Config `storage_type = "sqlite"`.<br>2. Start Server. | Logs show "Initializing SQLite Storage".<br>`.db` file created on disk. |
| **INT-02** | **TCPCL Listener** | 1. Config `tcpcl_port = 9999`.<br>2. Start Server. | Logs show "TCPCL listening on 0.0.0.0:9999".<br>`telnet localhost 9999` connects. |
| **INT-03** | **Management API** | 1. Config `management_port = 50051`.<br>2. Start Server. | Logs show "Management Service listening".<br>`grpcurl` or client can connect. |
| **INT-04** | **Health Check** | 1. Query `/health` (HTTP) or gRPC Health. | Returns `SERVING` / 200 OK. |

### 4.4 Performance & Scalability (REQ-13)

*Objective: Verify system compliance with critical performance metrics.*
*Harness: Dedicated performance environment (e.g., 2 nodes connected via 10GbE).*

| Test ID | Scenario | Procedure | Pass Criteria |
| ----- | ----- | ----- | ----- |
| **PERF-SYS-01** | **High Throughput (10Gbps)** | 1. Generate large bundles (10MB+).<br>2. Send via TCPCL over 10GbE link.<br>3. Measure goodput. | Throughput approaches 10Gbps (link saturation). |
| **PERF-SYS-02** | **Bundle Processing Rate** | 1. Generate small bundles (1KB).<br>2. Send burst of 100k bundles.<br>3. Measure processing time. | Rate > 1000 bundles/sec. |
| **PERF-SYS-03** | **Large Reassembly** | 1. Fragment a 4.1GB file into 10MB chunks.<br>2. Send fragments to server.<br>3. Verify reassembled payload. | 1. Reassembly succeeds.<br>2. MD5 checksum matches.<br>3. No OOM crash. |
| **PERF-SYS-04** | **Storage Scalability** | 1. Fill storage with 1 million small bundles (metadata stress).<br>2. Perform retrieval/deletion operations. | Operations remain O(1) or O(log n).<br>No significant latency degradation. |

### 4.5 Packaging & Deployment (REQ-15, REQ-16)

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
