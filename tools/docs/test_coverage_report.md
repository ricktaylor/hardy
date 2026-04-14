# Tools Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-tools` |
| **Standard** | — |
| **Test Plans** | [`PLAN-TOOLS-01`](test_plan.md), [`COMP-BPV7-CLI-01`](../../bpv7/docs/component_test_plan.md), [`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md) |
| **Date** | 2026-04-13 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

This crate provides the `bp` CLI binary (ping, bundle, cbor subcommands). CLI integration tests are defined in `bpv7/tools/tests/bundle_tools_test.sh` (26 tests). The `bp ping` subcommand is exercised by the interop test suite ([`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md)).

| Part 4 Ref | Requirement | Result | Verified By |
| :--- | :--- | :--- | :--- |
| 19.2 | Tools for testing correct functioning of the solution | **Pass** | 26 CLI integration tests + interop ping tests |
| 19.2.1 | Rate-controlled bundle sending | **Pass** | Interop test scripts (`bp ping -c N -i DURATION`) |
| 19.2.3 | Round-trip time reporting | **Pass** | Interop test scripts (`bp ping` RTT output) |
| 19.2.4 | Tools do not rely on status reports | **Pass** | Design review — uses echo service, not status reports |
| 19.2.5 | Tools run without a local BPA | **Pass** | Design review — standalone binary with embedded BPA |

## 2. Test Inventory

### Unit Tests

No unit tests are implemented — see [`PLAN-TOOLS-01`](test_plan.md) §4 for rationale.

### CLI Integration Tests

Test script: [`bpv7/tools/tests/bundle_tools_test.sh`](../../bpv7/tools/tests/bundle_tools_test.sh) — 26 tests exercising the `bundle` and `cbor` CLI tools. These tests drive the `bp` binary (built from this crate's dependencies) and verify end-to-end bundle operations.

| Suite | Tests | Coverage |
| :--- | :--- | :--- |
| Bundle Creation | 3 | CREATE-01..03: create, inspect, extract payload |
| Block Manipulation | 3 | add-block (hop-count, age), remove-block |
| Security (BIB) | 5 | Sign, verify, remove-integrity |
| Security (BCB) | 6 | Encrypt, inspect encrypted, extract with keys, remove-encryption |
| Validation | 2 | Validate plain + encrypted bundles |
| Rewrite & Canonicalization | 1 | Rewrite valid bundle |
| Pipeline Operations | 3 | create-sign-encrypt, decrypt-extract |
| Primary Block Security | 1 | Sign primary block with CRC |
| Error Handling | 1 | Reject invalid arguments |
| **Total** | **26** | |

## 3. Coverage vs Plan

Coverage is measured against [`PLAN-TOOLS-01`](test_plan.md). The CLI tests are also defined in the bpv7 component test plan and primarily verify bpv7 library behaviour through the CLI tool layer.

| Scope | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- |
| CLI integration (via bpv7 component plan) | 26 | 26 | Complete |
| Unit tests (CLI argument parsing, privilege checks) | — | 0 | No plan exists |

## 4. Line Coverage

Line coverage is not useful for this crate. The `bp` binary is ~100 lines of CLI dispatch code. All substantive logic lives in `hardy-bpv7`, `hardy-proto`, and `hardy-tcpclv4`, which have their own coverage reports.

## 5. Key Gaps

| Area | Gap | Severity | Notes |
| :--- | :--- | :--- | :--- |
| CLI wrappers | No unit tests for argument parsing or privilege checks | Low | Thin wrapper; logic verified end-to-end by CLI integration tests |
| Ping subcommand | No dedicated test coverage for `bp ping` | Low | Exercised by interop tests |

## 6. Conclusion

26 CLI integration tests verify end-to-end bundle operations through the `bp` tool, covering creation, inspection, signing, encryption, validation, rewriting, and pipeline workflows. These tests are attributed to the bpv7 component test plan and provide evidence for bpv7 LLR verification. No unit tests are implemented for the thin CLI wrapper. Line coverage is not applicable.
