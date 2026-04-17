# PICS to Test Case Mapping

| Document Info | Details |
| :--- | :--- |
| **Project** | Hardy DTN Router |
| **PICS Reference** | CCSDS 734.20-O-1 |
| **Version** | 1.1 |

## 1. Introduction

This document provides traceability between the CCSDS PICS (Protocol Implementation Conformance Statement) items and the test cases that verify each feature. This mapping enables verification that all implemented PICS items have corresponding test coverage.

## 2. Mapping Table

| PICS Item | Feature | Status | Support | Test Plan | Test ID(s) | Coverage |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | BP Formatting | M | Y | UTP-BPV7-01 | UT-PRI-*, UT-CAN-* | Full |
| 2 | Previous Node Receive | M | Y | UTP-BPV7-01 | UT-EXT-01 | Full |
| 3 | Previous Node Produce | O | Y | COMP-BPV7-CLI-01 | EXT-03 | Full |
| 4 | Bundle Age Receive | M | Y | UTP-BPV7-01 | UT-EXT-02 | Full |
| 5 | Bundle Age Produce | C | Y | COMP-BPV7-CLI-01 | EXT-05 | Full |
| 6 | Hop Count Receive | M | Y | UTP-BPV7-01 | UT-EXT-03 | Full |
| 7 | Hop Count Produce | O | Y | COMP-BPV7-CLI-01 | EXT-01, EXT-02 | Full |
| 8 | BPv7 Version | M | Y | UTP-BPV7-01 | UT-PRI-01 | Full |
| 9 | IPN Naming | M | Y | UTP-BPV7-01 | UT-EID-IPN-* | Full |
| 10 | Null Endpoint | O | Y | UTP-BPV7-01 | UT-EID-NULL-01 | Full |
| 11 | IPN Node No | M | Y | UTP-BPV7-01 | UT-EID-IPN-* | Full |
| 12 | IPN Service No | M | Y | UTP-BPV7-01 | UT-EID-IPN-* | Full |
| 13 | Bundle Creation Metadata | M | Y | UTP-BPA-01, PLAN-BPA-01 | §3.11 Age Fallback, Expiry Calculation; INT-BPA-01 | Planned |
| 14 | Bundle Send Request | M | Y | PLAN-BPA-01, PLAN-SVC-01 | INT-BPA-01, SVC-* | Full |
| 15 | Source Node ID | M | Y | UTP-BPA-01 | §3.10 Admin Resolution (IPN/DTN) | Planned |
| 16 | Registration Constraints | M | Y | UTP-BPA-01 | §3.4 Duplicate Reg, Cleanup; §3.2 Local Ephemeral | Planned |
| 17 | BPA Node Numbers | M | Y | UTP-BPA-01 | §3.10 Single Scheme Enforce, Invalid Types | Planned |
| 18 | BPA Endpoint Registration | M | N/A | - | - | N/A |
| 19 | Minimum Bundle Size | M | Y | PLAN-SERVER-01 | PERF-SYS-03 | Full |
| 20 | BPSec | O | Y | UTP-BPSEC-01 | UT-BIB-*, UT-BCB-* | Full |
| 21 | Service Interface | M | Y | PLAN-SVC-01 | SVC-* | Full |
| 22 | BP Node | M | Y | PLAN-SERVER-01 | SYS-*, INT-* | Full |
| 23 | TCP CLA | O.1 | Y | PLAN-TCPCL-01 | TCP-01 to TCP-10 | Full |
| 24 | LTP CLA | O.1 | N | - | - | Not Implemented |
| 25 | UDP CLA | O.1 | N | - | - | Not Implemented |
| 26 | Space Packets CLA | O.1 | N | - | - | Not Implemented |
| 27 | EPP CLA | O.1 | N | - | - | Not Implemented |
| 28 | BP Managed Information | M | N | - | - | **Gap - Not Implemented** |
| 29 | BP Data Structures | M | Y | UTP-BPV7-01 | UT-PRI-*, UT-CAN-* | Full |
| 30 | Block Structures | M | Y | UTP-BPV7-01 | UT-BLK-* | Full |
| 31 | Extension Blocks | M | Y | UTP-BPV7-01 | UT-EXT-* | Full |
| 32 | Generation of Admin Records | M | Y | UTP-BPA-01 | §3.1 Route Missing, TTL Expired | Planned |
| 33 | Bundle Transmission | M | Y | PLAN-BPA-01 | INT-BPA-01 | Full |
| 34 | Forwarding Contraindicated | M | Y | UTP-BPA-01 | §3.2 Local Ephemeral, Action Precedence; §3.1 Route Missing | Planned |
| 35 | Forwarding Failed | M | Y | UTP-BPA-01, PLAN-BPA-01 | §3.1 Route Missing; INT-BPA-01 | Planned |
| 36 | Forwarding Failed Return | O | Y | UTP-BPA-01 | §3.2 Reflection | Planned |
| 37 | Bundle Expiration | M | Y | UTP-BPA-01 | §3.11 Expiry Calculation; §3.9 Cache Ordering, Wakeup Trigger; §3.1 TTL Expired | Planned |
| 38 | Bundle Reception | M | Y | PLAN-BPA-01 | INT-BPA-02 | Full |
| 39 | Local Bundle Delivery | M | Y | PLAN-SVC-01 | SVC-RX-* | Full |
| 40 | Bundle Fragmentation | O | N | - | - | Not Implemented |
| 41 | Fragmentation Procedures | C | N | - | - | Not Implemented |
| 42 | ADU Reassembly | M | Y | PLAN-BPA-01 | INT-BPA-03 | Full |
| 43 | Bundle Deletion Report | O | Y | UTP-BPA-01 | §3.1 Route Missing, TTL Expired (status reports on deletion) | Planned |
| 44 | Bundle Deletion Constraints | M | Y | UTP-BPA-01 | §3.13 Unknown Block Drop/Keep; §3.6 Quota Enforcement | Planned |
| 45 | Discarding a Bundle | M | Y | UTP-BPA-01 | §3.2 Action Precedence (Drop); §3.6 Quota Enforcement, Eviction | Planned |
| 46 | Canceling a Transmission | O | Y | PLAN-TCPCL-01, FUZZ-BPA-01 | TCP-* (transfer cancel); pipeline stability | Planned |
| 47 | Administrative Records | C | Y | UTP-BPA-01 | UT-SR-* | Full |
| 48 | Bundle Status Reports | C | Y | UTP-BPA-01 | UT-SR-* | Full |
| 49 | Generating Admin Records | O | Y | UTP-BPA-01 | UT-SR-* | Full |

## 3. Coverage Summary

| Category | Total Items | Implemented (Y/N/A) | Fully Tested | Planned | Coverage |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Mandatory (M) | 32 | 31 (1 N/A) | 16 | 14 | ~52% verified, 97% mapped |
| Optional (O) | 10 | 7 | 5 | 2 | ~70% verified |
| Optional Group (O.1) | 5 | 1 | 1 | 0 | 100% of implemented |
| Conditional (C) | 3 | 3 | 3 | 0 | 100% |

*Note: 55/59 in-scope BPA unit plan scenarios are implemented (93%). 2 stubs remain (queue selection/fallback — post-initial-phase). See the unit test plan and [`bpa/docs/test_coverage_report.md`](../bpa/docs/test_coverage_report.md) for details.*

## 4. Gaps and Actions

### 4.1 Implementation Gaps

| PICS Item | Feature | Status | Support | Impact | Action |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 28 | BP Managed Information (Annex C) | M | N | Out of scope for initial phase | Documented as known limitation |

### 4.2 Test Implementation Gaps

BPA has 55/59 in-scope plan scenarios implemented (93%). 2 stubs remain in `bpa/src/cla/peers.rs` (queue selection/fallback — post-initial-phase scope).

## 5. Revision History

| Date | Version | Author | Changes |
| :--- | :--- | :--- | :--- |
| 2026-01-27 | 0.1 | Generated | Initial draft with partial mapping |
| 2026-03-14 | 0.2 | Generated | Resolved all 15 TBD entries by mapping to UTP-BPA-01/PLAN-BPA-01 test scenarios |
| 2026-04-01 | 0.3 | Generated | Corrected stub count (36 not 48), updated Item 28 status |
