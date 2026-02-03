# Unit Test Plan: EID Patterns

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Addressing & Routing Logic |
| **Module** | `hardy-eid-pattern` |
| **Requirements Ref** | [REQ-6](../../docs/requirements.md#req-6-time-variant-routing-api), [LLR 6.1.x](../../docs/requirements.md#311-eid-patterns-parent-req-6) |
| **Standard Ref** | `draft-ietf-dtn-eid-pattern-05` |
| **Test Suite ID** | UTP-PAT-01 |

## 1. Introduction

This document details the unit testing strategy for the `hardy-eid-pattern` module. This module provides the critical routing and filtering logic by defining how Endpoint IDs (EIDs) are matched against abstract patterns (e.g., `ipn:1.*`, `dtn://node/*`).

**Scope:**

* **String Parsing:** deserializing textual pattern representations into Rust structs.

* **Matching Logic:** Verifying if a specific `EID` is "contained" by a pattern.

* **Scheme Support:** Coverage for both `ipn` (Integer) and `dtn` (URI) schemes.

* **Complex Ranges:** Validation of ranges `[1-10]` and wildcards (`*`, `**`).

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by this plan:

| LLR ID | Description | Draft Reference |
 | ----- | ----- | ----- |
| **6.1.1** | The EID pattern parsing functionality shall correctly parse the textual representation of `ipn` and `dtn` patterns. | Sec 3 (Syntax) |
| **6.1.2** | The EID pattern parsing functionality shall provide a function to determine if a particular EID and EID pattern match. | Sec 4 (Matching) |

## 3. Unit Test Cases

The following scenarios are verified by the unit tests located in `eid-pattern/src/`.

### 3.1 IPN Pattern Parsing & Matching (LLR 6.1.1, 6.1.2)

*Objective: Verify integer-space pattern logic (Node & Service components).*

| Test Scenario | Source File | Input | Match Target | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Exact Match (3-element)** | `src/str_tests.rs` | `ipn:0.3.4` | `ipn:0.3.4` | **MATCH** |
| **Exact Mismatch** | `src/str_tests.rs` | `ipn:0.3.4` | `ipn:1.3.4` | NO MATCH |
| **Wildcard Service** | `src/str_tests.rs` | `ipn:0.3.*` | `ipn:0.3.9999` | **MATCH** |
| **Wildcard Node** | `src/str_tests.rs` | `ipn:0.*.4` | `ipn:0.999.4` | **MATCH** |
| **Full Wildcard (`**`)** | `src/str_tests.rs` | `ipn:**` | `ipn:123.456` | **MATCH** |
| **Range (Service)** | `src/str_tests.rs` | `ipn:0.3.[10-19]` | `ipn:0.3.15` | **MATCH** |
| **Range (Out of bounds)** | `src/str_tests.rs` | `ipn:0.3.[10-19]` | `ipn:0.3.9` | NO MATCH |
| **Multi-Range** | `src/str_tests.rs` | `ipn:0.3.[0-4,10-19]` | `ipn:0.3.10` | **MATCH** |
| **Coalescing Multi-Range** | `src/str_tests.rs` | `ipn:0.3.[0-9,10-19]` | `ipn:0.3.15` | **MATCH** |
| **Open-ended Range** | `src/str_tests.rs` | `ipn:0.3.[10+]` | `ipn:0.3.9999` | **MATCH** |
| **`ipn:!.*` Pattern** | `src/str_tests.rs` | `ipn:!.*` | `ipn:0.4294967295.0` | **MATCH** |
| **`ipn:!.*` Mismatch** | `src/str_tests.rs` | `ipn:!.*` | `ipn:0.3.1` | NO MATCH |
| **Legacy Format** | `src/str_tests.rs` | `ipn:1.2` | `ipn:1.2` | **MATCH** |

### 3.2 DTN Pattern Parsing & Matching (LLR 6.1.1, 6.1.2)

*Objective: Verify URI-string pattern logic (Authority & Path).*

| Test Scenario | Source File | Input | Match Target | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Exact URI** | `src/str_tests.rs` | `dtn://node/service` | `dtn://node/service` | **MATCH** |
| **Prefix Wildcard** | `src/str_tests.rs` | `dtn://node/*` | `dtn://node/foo` | **MATCH** |
| **Recursive Wildcard (Path)** | `src/str_tests.rs` | `dtn://node/**` | `dtn://node/a/b` | **MATCH** |
| **Recursive Wildcard (Authority)** | `src/str_tests.rs` | `dtn://**/some/serv` | `dtn://foo/some/serv` | **MATCH** |
| **None Pattern** | `src/str_tests.rs` | `dtn:none` | `dtn:none` | **MATCH** |
| **Scheme Wildcard** | `src/str_tests.rs` | `dtn:**` | `dtn://any` | **MATCH** |
| **Scheme Wildcard (Numeric)** | `src/str_tests.rs` | `1:**` | `dtn://any` | **MATCH** |

### 3.3 Set Pattern Parsing (LLR 6.1.1)

*Objective: Verify parsing of Union/Set patterns.*

| Test Scenario | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- |
| **Union Scheme** | `src/str_tests.rs` | `dtn://node/service\|ipn:0.3.4` | **PARSE OK** |
| **Any Scheme** | `src/str_tests.rs` | `*:**` | **PARSE OK** |

### 3.4 Invalid Pattern Syntax (Robustness)

*Objective: Verify parser rejects malformed patterns immediately.*

| Test Scenario | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- |
| **Bad Separator** | `src/str_tests.rs` | `ipn:1-1` | **Error** |
| **Inverted Range** | `src/str_tests.rs` | `ipn:[10-5].1` | **Error** |
| **Malformed Range** | `src/str_tests.rs` | `ipn:[10-].1` | **Error** |
| **Invalid Scheme** | `src/str_tests.rs` | `http://*` | **Error** |

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-eid-pattern`

* **Pass Criteria:** All tests listed above must return `ok`.

* **Coverage Target:** > 90% line coverage for `src/lib.rs`, `src/ipn_pattern.rs`, and `src/dtn_pattern.rs`.
