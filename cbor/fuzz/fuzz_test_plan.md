# Fuzz Test Plan: CBOR Decoding

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Data Serialization (Security & Robustness) |
| **Module** | `hardy-cbor` |
| **Target Source** | `cbor/fuzz/fuzz_targets/decode.rs` |
| **Tooling** | `cargo-fuzz` (libFuzzer) + `sanitizers` |
| **Test Suite ID** | FUZZ-CBOR-01 |

## 1. Introduction

This document details the fuzz testing strategy for the `hardy-cbor` module. Unlike unit tests which verify known happy/sad paths, fuzz testing feeds semi-random, mutated byte streams into the decoder to discover unhandled edge cases, memory safety violations, and denial-of-service vectors.

**Primary Objective:** Ensure that **NO** input sequence can cause the `hardy` router to crash (panic) or hang (timeout).

## 2. Requirements Mapping

The following requirements from **DTN-LLR_v1.1** are verified by this plan:

| LLR ID | Description |
| ----- | ----- |
| **REQ-14** | Fuzz testing of all external APIs. |
| **1.1.10** | CBOR decoder must parse items within context of Maps/Arrays correctly. |
| **1.1.12** | CBOR decoder must indicate if an incomplete item is found at end of buffer. |

## 3. Fuzz Target Definition

The primary target is defined in `cbor/fuzz/fuzz_targets/decode.rs`.

### 3.1 Target Logic

The harness performs the following operations on every iteration:

1. **Input:** Receives an arbitrary byte slice (`&[u8]`) from the fuzzer engine.

2. **Action:** Calls `hardy_cbor::decode()` attempting to parse the bytes into a generic CBOR `Value` AST.

3. **Invariant Check:** The decoder **MUST** return either `Ok(Value)` or `Err(DecodeError)`. It **MUST NOT** panic.

### 3.2 Coverage Scope

This target exercises the following internal paths:

* **Header Parsing:** Processing of Initial Bytes (Major Type + Additional Info).

* **Variable Length Integers:** Parsing of 1, 2, 4, and 8-byte integer arguments.

* **Recursion Depth:** Parsing of nested Arrays `[]` and Maps `{}`.

* **String Validation:** UTF-8 validation for Text Strings.

* **Resource Allocation:** Vector resizing based on declared lengths in Arrays/Maps/Byte Strings.

## 4. Vulnerability Classes & Mitigation

The fuzz target is specifically designed to uncover the following classes of vulnerabilities:

| Vulnerability Class | Description | Mitigation Strategy Verified |
 | ----- | ----- | ----- |
| **Stack Overflow** | Input contains deeply nested structures (e.g., `[[[[...]]]]`). | Verify decoder implements a recursion depth limit (default: 64/128). |
| **Memory Exhaustion (OOM)** | Input declares a huge length (e.g., "Array of 4 Billion items") but provides few bytes. | Verify decoder does not pre-allocate memory based on untrusted length headers. |
| **Panic on Invalid Data** | Input violates CBOR rules (e.g., reserved values, broken UTF-8). | Verify decoder returns structured `Err` instead of calling `unwrap()` or `expect()`. |
| **Infinite Loops** | Input constructs Indefinite Length items that never terminate. | Verify parser consumes bytes on every state transition and handles EOF correctly. |

## 5. Execution & Configuration

### 5.1 Running the Fuzzer

The following command executes the target using the standard Rust fuzzing infrastructure:

```bash
# Run for a set duration (Regression Mode)
cargo fuzz run decode -- -max_total_time=3600 # 1 Hour

# Run indefinitely (Discovery Mode)
cargo fuzz run decode -j $(nproc)
```

### 5.2 Sanitizer Configuration

To detect subtle memory errors that don't immediately crash, the target should run with AddressSanitizer (ASAN):

```bash
RUSTFLAGS="-Zsanitizer=address" cargo fuzz run decode
```

## 6. Pass/Fail Criteria

* **PASS:** The fuzzer runs for the defined duration (e.g., 24 hours) with **zero** artifacts created (no crashes).
* **FAIL:** The fuzzer creates a `crash-*` or `timeout-*` artifact.
  * **Action:** Analyze the artifact using `cargo fuzz fmt < artifact`.
  * **Action:** Replay the crash using `cargo run --bin decode-repro < artifact`.
  * **Remediation:** Fix the logic error and add the crash artifact to the permanent regression corpus.

## 7. Corpus Management

To improve efficiency, the fuzzer is seeded with a corpus of valid inputs.

* **Location:** `cbor/fuzz/corpus/decode/`
* **Seed Data:**
  * RFC 8949 Appendix A examples.
  * Valid IPN and DTN EID encodings.
  * Serialized bundles from Unit Tests.
