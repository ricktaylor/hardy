# Fuzz Test Plan: Bundle Protocol v7

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Bundle Protocol Parsing (Robustness) |
| **Module** | `hardy-bpv7` |
| **Target Directory** | `bpv7/fuzz/fuzz_targets/` |
| **Tooling** | `cargo-fuzz` (libFuzzer) + `sanitizers` |
| **Test Suite ID** | FUZZ-BPV7-01 |

## 1. Introduction

This document details the fuzz testing strategy for the `hardy-bpv7` module. This module is the "front line" of the router, accepting data from potentially untrusted sources (TCPCL peers). It must be resilient against malformed Endpoint IDs, corrupted Bundle structures, and malicious BPSec blocks.

**Primary Objective:** Ensure that parsing logic handles all byte sequences gracefully without crashing (panic) or allocating excessive resources (OOM).

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by this plan:

| LLR ID | Description |
| ----- | ----- |
| [**REQ-14**](../../docs/requirements.md#req-14-reliability) | Fuzz testing of all external APIs. |
| [**1.1.12**](../../docs/requirements.md#33-cbor-decoding-parent-req-1) | CBOR decoder must indicate if an incomplete item is found at end of buffer. |
| [**1.1.18**](../../docs/requirements.md#35-bpv7-parsing-parent-req-1) | Parser must not fail when presented with unrecognised but correctly encoded flags. |
| [**1.1.21**](../../docs/requirements.md#35-bpv7-parsing-parent-req-1) | Parser must parse and validate all CRC values. |
| [**1.1.30**](../../docs/requirements.md#37-bpv7-bundle-processing-parent-req-1) | Processing must enforce bundle rewriting rules when discarding unrecognised blocks. |

## 3. Fuzz Target Definitions

The strategy utilizes three distinct fuzz targets located in `bpv7/fuzz/fuzz_targets/`.

### 3.1 Target A: EID String Parsing

* **Source File:** `eid_string_parse.rs` (or equivalent)

* **Input:** Random UTF-8 String (`&str`).

* **Action:** Calls `Eid::from_str()` or `str::parse::<Eid>()`.

* **Goal:** Verify the parser handles malformed URIs, invalid IPN delimiters, and unexpected characters in schemes (e.g., `ipn:1.-2`, `dtn://\0`).

### 3.2 Target B: EID CBOR Decoding

* **Source File:** `eid_cbor_decode.rs` (or equivalent)

* **Input:** Random Byte Slice (`&[u8]`).

* **Action:** Calls the internal CBOR decoder for EIDs (expecting 2-element or 3-element Arrays, or CBOR Text Strings).

* **Goal:** Verify robustness against malformed CBOR structures claiming to be EIDs (e.g., wrong array length, integer overflows in IPN node IDs).

### 3.3 Target C: Full Bundle Parsing

* **Source File:** `bundle_parse.rs` (or equivalent)

* **Input:** Random Byte Slice (`&[u8]`).

* **Action:** Calls `Bundle::parse()` (or `hardy_bpv7::parse_bundle`).

* **Goal:** Verify the full parsing pipeline:

  * Primary Block decoding (Flags, Timestamps).

  * Extension Block parsing (Canonical Bundle Block Format).

  * Payload extraction.

  * **BPSec Processing:** Robustness of BIB/BCB parsing and decryption attempts (REQ-2).

  * **Rewriting Logic:** Handling of non-canonical blocks triggering `RewrittenBundle` (LLR 1.1.30).

  * CRC Validation logic (ensure CRC checks don't panic on buffer boundaries).

## 4. Vulnerability Classes & Mitigation

In addition to standard memory safety (Stack/Heap), these targets specifically hunt for:

| Vulnerability Class | Description | Mitigation Strategy Verified |
 | ----- | ----- | ----- |
| **Resource Exhaustion (Payload)** | Bundle header claims a 4GB payload length, but input provides 10 bytes. | Verify parser does not pre-allocate buffers based on untrusted length fields. |
| **EID Scheme Confusion** | Inputs mixing `ipn` and `dtn` semantics (e.g., `ipn://node`). | Verify strict validation of scheme-specific rules. |
| **Infinite Extension Loops** | Malformed headers causing the parser to read 0-length extension blocks indefinitely. | Verify parser advances the read cursor on every block processed. |
| **Integer Overflow (Time)** | Timestamp calculations (2000 Epoch + Offset) causing panic. | Verify checked arithmetic (`checked_add`) is used for time logic. |
| **CRC Buffer Overread** | CRC check reads past the end of the provided slice. | Verify bounds checking in CRC32C/CRC16 algorithms. |
| **BPSec Panic** | Malformed security blocks causing panics in crypto wrappers. | Verify `bpsec` errors are propagated, not unwrapped. |

## 5. Execution & Configuration

### 5.1 Running the Targets

Execute each target individually to isolate crashes.

```bash
# Target A: String EIDs
cargo fuzz run eid_string_parse -- -max_total_time=1800

# Target B: CBOR EIDs
cargo fuzz run eid_cbor_decode -- -max_total_time=1800

# Target C: Full Bundle (Heavy/Slow)
cargo fuzz run bundle_parse -j $(nproc) -- -max_len=1048576 # Allow up to 1MB inputs
```

### 5.2 Sanitizer Configuration

AddressSanitizer (ASAN) is critical for the Bundle Parser to detect off-by-one errors in block slicing.

```bash
RUSTFLAGS="-Zsanitizer=address" cargo fuzz run bundle_parse
```

## 6. Pass/Fail Criteria

* **PASS:** Zero crashes (panics/segfaults) and zero timeouts (>10s hang) over the execution period.
* **FAIL:** Any artifact generation (`crash-*` or `timeout-*`).

## 7. Corpus Management

* **Location:** `bpv7/fuzz/corpus/{target_name}/`
* **Seed Data:**
  * **EID String:** List of valid URIs (`ipn:1.1`, `dtn://node/svc`, `dtn:none`).
  * **EID CBOR:** Hex dumps of valid EIDs from RFC 9171 examples.
  * **Bundle:** "Golden Bundles" generated by `hardy-bpv7-tools` (e.g., `test_data/cli_01.bundle`).
