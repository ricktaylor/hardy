# Test Plan: CLI Tools (bp command)

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | CLI Tools |
| **Module** | `hardy-tools` |
| **Requirements Ref** | [REQ-19](../../docs/requirements.md#req-19-a-well-featured-suite-of-management-and-monitoring-tools), [LLR 19.2.x](../../docs/requirements.md#318-tools-parent-req-19) |
| **Test Suite ID** | PLAN-TOOLS-01 |
| **Version** | 1.0 |

## 1. Introduction

This document details the testing strategy for the `hardy-tools` crate, which provides the `bp` CLI binary. The `bp` command currently offers the `ping` subcommand for measuring bundle round-trip time. The crate is a thin CLI wrapper over `bpv7` and gRPC client logic; no unit tests are required.

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by the tests referenced in this plan:

| LLR ID | Description | Test Method |
| :--- | :--- | :--- |
| **19.2.1** | Rate-controlled bundle sending tool. | Interop test scripts. |
| **19.2.3** | Round-trip time reporting (`bp ping`). | Interop test scripts. |
| **19.2.4** | Tools do not rely on BPv7 status reports. | Design review (uses echo service, not status reports). |
| **19.2.5** | Tools run without a local BPA. | Design review (standalone binary, connects via gRPC). |

## 3. Test Coverage

### Bundle Tool Integration Tests

The `bpv7/tools/tests/bundle_tools_test.sh` script provides 26 integration tests covering the underlying bundle manipulation commands (create, inspect, sign, encrypt, validate, pipeline). These verify the `bpv7` tooling that `bp` depends on.

### Interop Tests

The `bp ping` subcommand is exercised by the interop test scripts ([`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md)), which verify end-to-end round-trip against 7 peer implementations.

## 4. Unit Test Cases

No unit tests are planned. The crate is a CLI dispatcher (clap `Parser` + `Subcommand`) that delegates to `bpv7` for bundle construction and gRPC for transport. Both are tested independently.

## 5. Execution Strategy

* **Bundle Tool Tests:** `bpv7/tools/tests/bundle_tools_test.sh`
* **Interop Tests:** [`tests/interop/run_all.sh`](../../tests/interop/run_all.sh) (see [`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md))
