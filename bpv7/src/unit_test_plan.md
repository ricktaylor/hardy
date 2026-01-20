# Unit Test Plan: Bundle Protocol v7 (BPv7)

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Bundle Protocol Parser/Serializer (Syntax) |
| **Module** | `hardy-bpv7` |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-1), `DTN-LLR_v1.1` (Section 1.2) |
| **Standard Ref** | RFC 9171 (BPv7 Wire Format), RFC 9758 (IPN Scheme) |
| **Test Suite ID** | UTP-BPV7-01 |

## 1. Introduction

This document details the unit testing strategy for the `hardy-bpv7` functional area. This module is strictly responsible for the **parsing and serialization** of the BPv7 wire format.

**Scope:**

* **Syntax Validation:** Ensuring byte streams match RFC 9171 structures.

* **Canonicalization:** Ensuring serialized bundles strictly follow block ordering rules.

* **Compliance:** Verification of CCSDS Space Profile constraints (LLR 1.1.1).

* **Parsing Modes:** Verification of both "Strict" (Compliance) and "Rewriting" (Robustness) parsing capabilities.

* **Factories:** Verification of Builder and Editor patterns for programmatic bundle manipulation.

## 2. Requirements Mapping

The following requirements from **DTN-LLR_v1.1** are verified by this plan:

| LLR ID | Description | RFC Ref |
 | ----- | ----- | ----- |
| **1.1.1** | Compliant with all mandatory requirements of CCSDS Bundle Protocol. | CCSDS 734.20-O-1 |
| **1.1.12** | CBOR decoder must indicate if an incomplete item is found at end of buffer. | - |
| **1.1.14** | Parser must indicate when bundle rewriting has occurred. | RFC 9171 Sec 5.6 |
| **1.1.15** | Parser must indicate that the Primary Block is valid. | RFC 9171 Sec 4.2 |
| **1.1.19** | Parser must parse/validate extension blocks specified in RFC 9171. | RFC 9171 Sec 4.4 |
| **1.1.21** | Parser must parse and validate all CRC values. | RFC 9171 Sec 4.2 |
| **1.1.22** | Parser must support all CRC types specified in RFC 9171. | RFC 9171 Sec 4.2 |
| **1.1.23, 1.1.24** | Parser must support `ipn` scheme EIDs (2 and 3-element) and `dtn` scheme. | RFC 9171 Sec 4.2.5, RFC 9758 |
| **1.1.25** | Generator must create valid, canonical CBOR encoded bundles, including correct block ordering. | RFC 9171 Sec 4.1, 4.3 |
| **1.1.30** | Processing must enforce bundle rewriting rules when discarding unrecognised blocks. | RFC 9171 Sec 5.6 |
| **1.1.33** | Processing must use Bundle Age block for expiry if Creation Time is zero. | RFC 9171 Sec 4.4.2 |
| **1.1.34** | Processing must process and act on Hop Count extension block. | RFC 9171 Sec 4.4.3 |

## 3. Unit Test Cases

The following scenarios are verified by the unit tests located in `bpv7/src/`.

### 3.1 Primary Block & Structure (LLR 1.1.1, 1.1.15, 1.1.25)

*Objective: Verify parsing of address schemes (CBOR & String) per LLR 1.1.23 and 1.1.24.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **IPN Legacy Parsing** | Parse standard 2-element IPN EID. | `src/eid/cbor_tests.rs` | CBOR `[1, 2]` | `ipn:1.2` |
| **IPN Modern Parsing** | Parse RFC 9758 3-element IPN EID. | `src/eid/cbor_tests.rs` | CBOR `[1, 2, 3]` | `ipn:1.2.3` |
| **DTN Scheme Parsing** | Parse standard URI string. | `src/eid/cbor_tests.rs` | CBOR `dtn://node/svc` | `dtn://node/svc` |
| **Null Endpoint Parsing** | Parse the Null EID. | `src/eid/cbor_tests.rs` | CBOR `dtn:none` / `ipn:0.0` | `Eid::Null` |
| **Invalid EID Rejection** | Verify rejection of malformed EIDs. | `src/eid/cbor_tests.rs` | Malformed CBOR | Error: `InvalidCBOR` / `IpnInvalid...` |
| **String Parsing (IPN)** | Parse IPN string formats. | `src/eid/str_tests.rs` | `ipn:1.2`, `ipn:1.2.3` | Valid `Eid` |
| **String Parsing (DTN)** | Parse DTN string formats. | `src/eid/str_tests.rs` | `dtn://node/svc` | Valid `Eid` |
| **String Parsing (Errors)** | Reject invalid EID strings. | `src/eid/str_tests.rs` | `ipn:`, `dtn:` | Error |
| **EID Roundtrip** | Verify EID serialization roundtrip. | `src/eid/roundtrip_tests.rs` | Various EIDs | Output matches input. |
| **Invalid Flag Combination** | Verify rejection of bundles with invalid flag combinations. | `src/bundle/parse.rs` | Hex Stream | `RewrittenBundle::Invalid` / `Error::InvalidFlags` |
| **CCSDS Compliance** | Verify full compliance with CCSDS profile (e.g. no floats). (LLR 1.1.1) | TODO | Various | Success/Error |
| **Primary Block Validation** | Verify Primary Block validation logic. (LLR 1.1.15) | TODO | Valid/Invalid PB | Success/Error |
| **CRC Validation** | Verify CRC validation (Valid/Invalid). (LLR 1.1.21) | TODO | Bundles with CRCs | Success/Error |
| **CRC Types** | Verify support for 16/32-bit CRCs. (LLR 1.1.22) | TODO | Bundles with various CRCs | Success |

### 3.2 Bundle Factories (Builder & Editor) (LLR 1.1.25)

*Objective: Verify programmatic creation and modification of bundles via the API.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Builder Minimal** | Create a basic bundle (Source, Dest, Payload). | `src/builder.rs` | `Builder::new(...)` | Valid `Bundle` struct; `build()` succeeds. |
| **Builder from Template** | Create a builder from a JSON template (Serde). | `src/builder.rs` | JSON Template | Valid `Builder`; `build()` succeeds. |

### 3.3 Extension Blocks & Processing (LLR 1.1.14, 1.1.19, 1.1.30, 1.1.33, 1.1.34)

*Objective: Verify parsing and processing logic for extension blocks and bundle rewriting.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Bundle Age Expiry** | Verify rejection if Creation Time is zero and Bundle Age missing. (LLR 1.1.33) | TODO | Bundle (Time=0, No Age) | Error: `MissingBundleAge` |
| **Hop Count** | Verify Hop Count parsing and limit checks. (LLR 1.1.34) | TODO | Bundle with HopCount | Parsed/Error if exceeded |
| **Bundle Rewriting** | Verify successful bundle rewriting (e.g. reordering). (LLR 1.1.14) | TODO | Non-canonical Bundle | `RewrittenBundle::Rewritten` |
| **Extension Parsing** | Verify parsing of PreviousNode, BundleAge, HopCount. (LLR 1.1.19) | TODO | Bundle with Ext Blocks | Valid `Bundle` fields |
| **Rewrite Rules** | Verify rewriting rules when discarding blocks. (LLR 1.1.30) | TODO | Bundle with Unknown Block | Rewritten Bundle |

### 3.4 Error Handling & Edge Cases (LLR 1.1.12)

*Objective: Verify robustness against malformed or incomplete data.*

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Truncated Bundle** | Verify handling of incomplete bundle data. (LLR 1.1.12) | TODO | Truncated Bytes | Error: `NeedMoreData` |
| **Trailing Data** | Verify rejection of bundles with trailing bytes. | TODO | Bundle + Extra Bytes | Error: `AdditionalData` |

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-bpv7`

* **Pass Criteria:** All tests listed above must return `ok`.

* **Coverage Target:** > 85% line coverage for `src/lib.rs`, `src/bundle/`, `src/block/`, `src/eid/`, `src/builder.rs`, and `src/editor.rs`.
