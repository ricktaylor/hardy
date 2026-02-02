# PICS to Test Case Mapping

| Document Info | Details |
| :--- | :--- |
| **Project** | Hardy (Cloud-based DTN Router) |
| **PICS Reference** | CCSDS 734.20-O-1 |
| **Status** | DRAFT - Requires detailed review |

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
| 13 | Bundle Creation Metadata | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 14 | Bundle Send Request | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 15 | Source Node ID | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 16 | Registration Constraints | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 17 | BPA Node Numbers | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
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
| 32 | Generation of Admin Records | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 33 | Bundle Transmission | M | Y | PLAN-BPA-01 | INT-BPA-01 | Full |
| 34 | Forwarding Contraindicated | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 35 | Forwarding Failed | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 36 | Forwarding Failed Return | O | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 37 | Bundle Expiration | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 38 | Bundle Reception | M | Y | PLAN-BPA-01 | INT-BPA-02 | Full |
| 39 | Local Bundle Delivery | M | Y | PLAN-SVC-01 | SVC-RX-* | Full |
| 40 | Bundle Fragmentation | O | N | - | - | Not Implemented |
| 41 | Fragmentation Procedures | C | N | - | - | Not Implemented |
| 42 | ADU Reassembly | M | Y | PLAN-BPA-01 | INT-BPA-03 | Full |
| 43 | Bundle Deletion Report | O | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 44 | Bundle Deletion Constraints | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 45 | Discarding a Bundle | M | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 46 | Canceling a Transmission | O | Y | UTP-BPA-01 | *TBD* | *TBD* |
| 47 | Administrative Records | C | Y | UTP-BPA-01 | UT-SR-* | Full |
| 48 | Bundle Status Reports | C | Y | UTP-BPA-01 | UT-SR-* | Full |
| 49 | Generating Admin Records | O | Y | UTP-BPA-01 | UT-SR-* | Full |

## 3. Coverage Summary

| Category | Total Items | Implemented (Y/N/A) | Fully Tested | Partial/TBD | Coverage |
| :--- | :--- | :--- | :--- | :--- | :--- |
| Mandatory (M) | 32 | 31 (1 N/A) | 15 | 15 | ~50% verified |
| Optional (O) | 10 | 7 | 5 | 2 | ~70% verified |
| Optional Group (O.1) | 5 | 1 | 1 | 0 | 100% of implemented |
| Conditional (C) | 3 | 3 | 3 | 0 | 100% |

*Note: Items marked TBD require detailed review of the referenced test plan to identify specific test IDs.*

## 4. Gaps and Actions

### 4.1 Implementation Gaps

| PICS Item | Feature | Status | Impact | Action |
| :--- | :--- | :--- | :--- | :--- |
| 28 | BP Managed Information | M/N | Required for custody transfer | Document as known limitation or implement |

### 4.2 Test Coverage Gaps

Items marked *TBD* in the mapping table require:
1. Review of the referenced test plan
2. Identification of specific test IDs that cover the PICS item
3. Update of this mapping document

## 5. Revision History

| Date | Version | Author | Changes |
| :--- | :--- | :--- | :--- |
| 2026-01-27 | 0.1 | Generated | Initial draft with partial mapping |
