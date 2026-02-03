# Unit Test Plan: CBOR Encoding & Decoding

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Data Serialization (CBOR) |
| **Module** | `hardy-cbor` |
| **Requirements Ref** | [REQ-1](../../docs/requirements.md#req-1-full-compliance-with-rfc9171), [LLR 1.1.x](../../docs/requirements.md#32-cbor-encoding-parent-req-1) |
| **Standard Ref** | RFC 8949 (CBOR) |
| **Test Suite ID** | UTP-CBOR-01 |

## 1. Introduction

This document details the unit testing strategy for the `hardy-cbor` functional area. This module is responsible for the low-level serialization and deserialization of data primitives used by the Bundle Protocol.

**Scope:**

* Verification of RFC 8949 compliance using standard "Appendix A" worked examples.

* Encoding/Decoding of integers, strings, arrays, maps.

* Malicious input detection (e.g., non-canonical integers).

* **Note:** RFC 9171 specific constraints (e.g., forbidding floats) are enforced by the consuming `hardy-bpv7` module and are not covered in this low-level unit test plan.

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by this plan:

| LLR ID | Description | RFC Ref |
 | ----- | ----- | ----- |
| **1.1.2, 1.1.8** | Encoder must emit tagged/untagged types; Decoder must report tags. | RFC 8949 Sec 3.4 |
| **1.1.3, 1.1.9** | Encoder/Decoder must support all major types. | RFC 8949 Sec 3.1 |
| **1.1.4** | Encoder must emit primitives in canonical form. | RFC 8949 Sec 4.2 |
| **1.1.5** | Decoder must handle indefinite length arrays/maps safely. | RFC 8949 Sec 3.2.2 |
| **1.1.7** | Decoder must report if a parsed data item is in canonical form. | RFC 8949 Sec 4.2 |
| **1.1.10** | CBOR decoder must parse items within context of Maps/Arrays correctly. | RFC 8949 Sec 3.2 |
| **1.1.11** | Decoder must support opportunistic parsing (try-parse). | - |
| **1.1.12** | Decoder must indicate if an incomplete item is found. | - |

## 3. Unit Test Cases

The following scenarios are verified by the unit tests located in `cbor/src/`.

### 3.1 Integer Canonicalization (LLR 1.1.4, 1.1.7)

*Objective: Verify strict adherence to shortest-form encoding rules (Deterministic Encoding).*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Minimal Width Encoding** | Check minimal encoding width. | `src/encode_tests.rs` | `23` (fits in 1 byte) | Byte: `0x17` (not `0x1817`) |
| **Boundary Transition** | Check boundary transition where width increases. | `src/encode_tests.rs` | `24` | Bytes: `0x1818` |
| **Non-Canonical Detection** | Feed over-long encoding to decoder. | `src/decode_tests.rs` | `0x1817` (value 23 encoded as u8) | Successful parse of `23`, but `canonical` flag is `false`. |

### 3.2 Indefinite Length Handling (LLR 1.1.5)

*Objective: Verify streaming decoding robustness.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Indefinite Array Parsing** | Parse indefinite array `[_ ... ]`. | `src/decode_tests.rs` | `0x9F0102FF` | `vec![1, 2]` |
| **Nested Indefinite Structures** | Parse nested indefinite structures. | `src/decode_tests.rs` | `0x9F9F01FF02FF` | `vec![vec![1], 2]` |
| **Unterminated Stream Detection** | Input ends before `0xFF` break. | `src/decode_tests.rs` | `0x9F0102` | Error: `NeedMoreData` |

### 3.3 Tagged Items and Opportunistic Parsing (LLR 1.1.2, 1.1.8, 1.1.11)

*Objective: Verify handling of semantic tags and robust iterator-style parsing.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Tagged Value Encoding** | Encode a value with a semantic tag. | `src/encode_tests.rs` | `Tag(32, "http://a.com")` | `0xD82072687474703A2F2F612E636F6D` |
| **Tagged Value Decoding** | Decode a tagged value and verify the tag is reported. | `src/decode_tests.rs` | `0xD820...` | Value is `"http://a.com"`, reported tags are `[32]`. |
| **Opportunistic `try_parse`** | Use `try_parse` on a sequence to get `Some(item)` then `None`. | `src/decode_tests.rs` | `0x8101` (Array `[1]`) | First call returns `Some(1)`, second call returns `None`. |

### 3.4 RFC 8949 Appendix A Compliance (Standard Examples)

*Objective: Verify bit-exact matching of standard CBOR examples for interoperability.*

| Test Scenario | Description | Source File | RFC Ref | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Standard Integers** | Encode/Decode standard integers. | `src/encode_tests.rs` | Appx A.1 | `23` -> `0x17` |
| **Standard Byte Strings** | Encode/Decode byte strings. | `src/encode_tests.rs` | Appx A.3 | `h'01020304'` -> `0x4401020304` |
| **Standard Text Strings** | Encode/Decode UTF-8 strings. | `src/encode_tests.rs` | Appx A.4 | `"IETF"` -> `0x6449455446` |
| **Standard Arrays** | Encode/Decode arrays of mixed types. | `src/encode_tests.rs` | Appx A.5 | `[1, 2, 3]` -> `0x83010203` |
| **Standard Maps** | Encode/Decode mixed maps. | `src/encode_tests.rs` | Appx A.6 | `{"a": 1, "b": [2, 3]}` -> `0xA26161016162820203` |
| **Standard Floats** | Encode/Decode floating point values. | `src/encode_tests.rs` | Appx A.7 | `1.1` -> `0xFB3FF199999999999A` |

### 3.5 Additional Type and Edge Case Coverage (LLR 1.1.3, 1.1.9)

*Objective: Ensure robust handling of all supported CBOR types and common edge cases found in the implementation.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Simple & Boolean Values** | Encode/Decode `true`, `false`, `null`, and `undefined`. | `src/decode_tests.rs` | `0xf5` | `true` |
| **Floating Point Edge Cases** | Encode/Decode `Infinity`, `-Infinity`, and `NaN` for f16, f32, and f64. | `src/decode_tests.rs` | `0xf97c00` | `f16::INFINITY` |
| **Empty Structures** | Encode/Decode empty arrays, maps, byte strings, and text strings. | `src/decode_tests.rs` | `0x80` | Empty array |
| **Unsupported Type Rejection** | Ensure the decoder rejects unsupported types like BIGNUMs (tags 2 and 3). | `src/decode_tests.rs` | `0xc249...` | `Error` |

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-cbor`

* **Pass Criteria:** All tests listed above must return `ok`.

* **Coverage Target:** > 90% line coverage for `src/lib.rs`, `src/encode.rs`, `src/decode.rs`, and `src/decode_seq.rs`.
