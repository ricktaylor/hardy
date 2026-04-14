# Test Plan: Bundle Echo Service

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Bundle Echo Service |
| **Module** | `echo-service` |
| **Implements** | `hardy_bpa::services::Service` trait |
| **Standard** | [draft-taylor-dtn-echo-service](https://datatracker.ietf.org/doc/draft-taylor-dtn-echo-service/) |
| **Requirements Ref** | None (application-level service, no LLRs) |
| **Test Suite ID** | PLAN-ECHO-01 |
| **Version** | 1.0 |

## 1. Introduction

This document details the testing strategy for the `echo-service` crate (~117 lines). This service implements draft-taylor-dtn-echo-service: it receives bundles at a well-known endpoint, swaps source and destination EIDs via the bpv7 `Editor`, and sends the response bundle back through the BPA pipeline.

## 2. Requirements Mapping

This crate implements draft-taylor-dtn-echo-service. There are no formal LLRs assigned to this crate. Correct behaviour is verified through end-to-end interop testing.

## 3. Test Coverage

### Interop Tests

The echo service is registered by `bpa-server` and exercised by the interop test suite. All 7 interoperating implementations send bundles to Hardy's echo service endpoint and verify the response, confirming correct bundle reflection through the full BPA pipeline.

See [`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md) for test scenarios and [`tests/interop/README.md`](../../tests/interop/README.md) for run instructions.

### Unit Tests

No unit tests are planned. The crate is a thin adapter between the `Service` trait and the bpv7 `Editor` API, both of which have their own test coverage. The interop suite provides sufficient end-to-end verification of the echo behaviour.

## 4. Execution Strategy

* **Interop Tests:** `tests/interop/<implementation>/test_*_ping.sh` (see [`PLAN-INTEROP-01`](../../tests/interop/docs/test_plan.md))
