# Test Plan: TCPCLv4 Server Application

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Application Runtime & Transport |
| **Module** | `hardy-tcpclv4-server` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-3, REQ-13, REQ-15, REQ-16), `DTN-LLR_v1.1` (Section 3) |
| **Test Suite ID** | PLAN-TCPCL-SERVER-01 |

## 1. Introduction

This document details the testing strategy for the `hardy-tcpclv4-server` module. This module is the **deployable executable** for the TCP Convergence Layer. It wraps the `hardy-tcpclv4` library, handling configuration, gRPC registration with the BPA, and process lifecycle.

**Scope:**

* **Configuration Management:** Loading settings from TOML/Env.
* **Process Lifecycle:** Startup, Shutdown, Signal handling.
* **BPA Integration:** gRPC registration and keepalive.
* **Packaging:** OCI Images and Helm Charts.

**Delegation:**
Core protocol logic (RFC 9174 state machine, packet parsing) is verified by the `hardy-tcpclv4` component test plan ([`PLAN-TCPCL-01`](../../tcpclv4/docs/component_test_plan.md)). This plan focuses on the server wrapper.

## 2. Requirements Mapping

The following requirements from **DTN-LLR_v1.1** are verified by the unit tests in this plan:

| LLR ID | Description |
| :--- | :--- |
| **3.1.5** | Allow configuration of default session parameters (Keepalive, Segment Size). |

## 3. Unit Test Cases

### 3.1 Configuration Logic (LLR 3.1.5)

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Default Load (CFG-01)** | Verify loading defaults when no config provided. | `src/config.rs` | No config file. | Valid config with default port 4556. |
| **TOML Parsing (CFG-02)** | Verify loading from TOML file. | `src/config.rs` | `hardy-tcpclv4.toml` with overrides. | Config struct matches file values. |
| **Env Override (CFG-03)** | Verify environment variables override config. | `src/config.rs` | Env `HARDY_TCPCLV4_BPA_ADDRESS`. | Config reflects env var value. |

## 4. System Test Cases (Black-Box Execution)

*Scope: Running the compiled binary `target/debug/hardy-tcpclv4-server`.*

### 4.1 Lifecycle & Integration

*Objective: Verify the server starts, connects to BPA, and shuts down.*

| Test ID | Scenario | Procedure | Expected Result |
| :--- | :--- | :--- | :--- |
| **SYS-01** | **Startup & Listen** | 1. Run `./hardy-tcpclv4-server`.<br>2. Check `netstat` for listening ports. | Process runs.<br>TCP port 4556 is LISTEN. |
| **SYS-02** | **BPA Registration** | 1. Start Mock BPA (gRPC).<br>2. Start Server.<br>3. Check mock BPA received message. | Server connects to BPA.<br>Sends `RegisterClaRequest`. |
| **SYS-03** | **Graceful Shutdown** | 1. Start Server.<br>2. Send `SIGINT` to process. | Logs show "Shutting down".<br>Process exits 0. |

### 4.2 Performance (REQ-13)

*Objective: Verify throughput capabilities of the standalone server.*

| Test ID | Scenario | Procedure | Pass Criteria |
| ----- | ----- | ----- | ----- |
| **PERF-SRV-01** | **Throughput** | 1. Run Server.<br>2. Connect `iperf`-like load generator via TCPCL. | Throughput > 1Gbps (or link limit). |

### 4.3 Packaging & Deployment (REQ-15, REQ-16)

*Objective: Verify build artifacts.*

| Test ID | Scenario | Procedure | Pass Criteria |
| ----- | ----- | ----- | ----- |
| **PKG-OCI-01** | **Image Structure** | Inspect image layers and metadata. | Base image is `distroless` or minimal.<br>Entrypoint is `hardy-tcpclv4-server`.<br>Non-root user. |
| **PKG-HELM-01** | **Chart Install** | Install Helm chart. | Pod starts, Readiness probe passes. |

## 5. Execution Strategy

* **Unit Tests:** `cargo test -p hardy-tcpclv4-server`
* **System Tests:** Manual verification or Python harness wrapping Docker.
