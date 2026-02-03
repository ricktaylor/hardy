# Fuzz Test Plan: EID Patterns

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Addressing & Routing (Robustness) |
| **Module** | `hardy-eid-pattern` |
| **Target Source** | `eid-patterns/fuzz/fuzz_targets/eid_pattern_str.rs` |
| **Tooling** | `cargo-fuzz` (libFuzzer) + `sanitizers` |
| **Test Suite ID** | FUZZ-PAT-01 |

## 1. Introduction

This document details the fuzz testing strategy for the `hardy-eid-pattern` module. As this module implements a text-based DSL (Domain Specific Language) for routing logic—including ranges `[]` and wildcards `*`/`**`—it is highly susceptible to parser edge cases.

**Primary Objective:** Verify that the `EidPattern::from_str` parser handles arbitrary text input safely, returning structured Errors rather than panicking, crashing, or entering infinite loops.

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by this plan:

| LLR ID | Description |
| ----- | ----- |
| [**REQ-14**](../../docs/requirements.md#req-14-reliability) | Fuzz testing of all external APIs. |
| [**6.1.1**](../../docs/requirements.md#311-eid-patterns-parent-req-6) | Correctly parse textual representation of `ipn` and `dtn` EID patterns. |

## 3. Fuzz Target Definition

The strategy utilizes the specific target defined in `eid-patterns/fuzz/fuzz_targets/eid_pattern_str.rs`.

### 3.1 Target Logic

The harness performs the following operations on every iteration:

1. **Input:** Receives an arbitrary UTF-8 string slice (`&str`) from the fuzzer engine.

2. **Action:** Calls `EidPattern::from_str(input)` (or `input.parse::<EidPattern>()`).

3. **Invariant Check:** The parser **MUST** return `Result<EidPattern, ParseError>`. It **MUST NOT** panic (e.g., index out of bounds on string slicing).

### 3.2 Coverage Scope

This target exercises the specific parsing logic for `draft-ietf-dtn-eid-pattern-05`:

* **Tokenization:** Handling of delimiters `:`, `.`, `/`, `[`, `]`.

* **Integer Parsing:** Conversion of substrings to `u64` (IPN nodes/services).

* **Range Logic:** Validation of range bounds (start <= end).

* **Wildcard Logic:** Handling of single (`*`) and double (`**`) wildcards.

* **Scheme Dispatch:** Distinguishing `ipn:` vs `dtn:` vs invalid schemes.

## 4. Vulnerability Classes & Mitigation

The fuzz target is specifically designed to uncover the following classes of vulnerabilities common in text parsers:

| Vulnerability Class | Description | Mitigation Strategy Verified |
 | ----- | ----- | ----- |
| **Index Out of Bounds** | Parser assumes fixed offsets for delimiters (e.g., assuming a `.` always follows `ipn:`). | Verify safe string slicing and iterator usage. |
| **Integer Overflow** | Input contains numbers larger than `u64::MAX` (e.g., `ipn:999999999999999999999...`). | Verify use of checked parsing or error handling on overflow. |
| **Empty/Inverted Ranges** | Inputs like `ipn:[10-1]` or `ipn:[]`. | Verify logic validation returns `Err` rather than panic. |
| **Wildcard Confusion** | Inputs mixing `*` and ranges inappropriately (e.g., `ipn:[*]`). | Verify parser rejects invalid syntax cleanly. |

## 5. Execution & Configuration

### 5.1 Running the Fuzzer

Execute the target using the standard Rust fuzzing infrastructure:

```bash
# Run for a set duration (Regression Mode)
cargo fuzz run eid_pattern_str -- -max_total_time=1800 # 30 Mins

# Run indefinitely (Discovery Mode)
cargo fuzz run eid_pattern_str -j $(nproc)
```

### 5.2 Sanitizer Configuration

AddressSanitizer (ASAN) is recommended to catch subtle buffer read overflows during string processing.

```bash
RUSTFLAGS="-Zsanitizer=address" cargo fuzz run eid_pattern_str
```

## 6. Pass/Fail Criteria

* **PASS:** The fuzzer runs for the defined duration with **zero** crashes.
* **FAIL:** The fuzzer creates a `crash-*` or `timeout-*` artifact.
  * **Action:** Analyze artifact with `cargo fuzz fmt < artifact`.
  * **Remediation:** Ensure `from_str` returns `Err` for the discovered input.

## 7. Corpus Management

To speed up discovery, the fuzzer should be seeded with valid patterns.

* **Location:** `eid-patterns/fuzz/corpus/eid_pattern_str/`
* **Seed Data:**
  * `ipn:1.1`
  * `ipn:1.*`
  * `ipn:[1-100].0`
  * `dtn://node/svc`
  * `dtn://node/*`
  * `dtn://node/**`
