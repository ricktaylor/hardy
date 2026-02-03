# Fuzz Test Plan: BPA Pipeline

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Bundle Processing Pipeline (Async Logic) |
| **Module** | `hardy-bpa` |
| **Target Source** | `bpa/fuzz/fuzz_targets/bpa.rs` |
| **Tooling** | `cargo-fuzz` (libFuzzer) + `tokio` (single thread) |
| **Test Suite ID** | FUZZ-BPA-01 |

## 1. Introduction

This document details the fuzz testing strategy for the `hardy-bpa` module. Unlike the parser fuzzers (which test data formats), this fuzzer targets the **Asynchronous Processing Pipeline**. It verifies that the BPA's internal state machine remains stable when bombarded with random sequences of events (Bundle Arrival, Timer Expiry, Route Changes).

**Primary Objective:** Ensure the BPA never enters a deadlock state, panics due to invalid state transitions, or corrupts its internal store under high-velocity event streams.

## 2. Requirements Mapping

The following requirements from **[requirements.md](../../docs/requirements.md)** are verified by this plan:

| ID | Description |
 | ----- | ----- |
| [**REQ-6**](../../docs/requirements.md#req-6-time-variant-routing-api) | Time-variant Routing API (Route updates during processing). |
| [**REQ-14**](../../docs/requirements.md#req-14-reliability) | Fuzz testing of all external APIs (Ingress/Egress). |
| [**1.1.33**](../../docs/requirements.md#37-bpv7-bundle-processing-parent-req-1) | Bundle Expiry (Time-to-Live processing). |
| [**1.1.34**](../../docs/requirements.md#37-bpv7-bundle-processing-parent-req-1) | Hop Count limit enforcement. |
| **1.1.50** | Status Report generation (triggered by `Action::Drop` routes in harness). |
| [**6.1.7**](../../docs/requirements.md#313-routing-parent-req-6) | Routing logic (Discard/Forward decisions). |
| [**6.1.8**](../../docs/requirements.md#313-routing-parent-req-6) | Bundle Reflection logic (triggered by `Action::Reflect` routes in harness). |

## 3. Fuzz Target Definition

The primary target is defined in `bpa/fuzz/fuzz_targets/bpa.rs`, utilizing definitions from `bpa/fuzz/src/lib.rs`.

### 3.1 Target Logic

The harness operates by simulating a "Mock Reactor" around the BPA:

1. **Setup:** Initializes a `Bpa` instance with in-memory storage and mock channels. Configures static routes (Wildcard, Drop, Reflect) to exercise routing logic without runtime reconfiguration.

2. **Input:** Receives an arbitrary byte slice (`&[u8]`) which is deserialized into a sequence of `Msg` enum variants (via `hardy_bpa_fuzz`).

3. **Execution:** The harness iterates through the list of `Msg` events:

   * **`Msg::Cla(cla::RandomBundle)`**: Injects a structurally valid, randomly generated Bundle struct into the CLA ingress. This tests the BPA logic handling of headers/extensions (e.g., Hop Count, Expiry) without parser failures.

   * **`Msg::ClaBytes(Vec<u8>)`**: Injects raw random bytes into the CLA ingress. This tests the parser integration and resilience against malformed wire data entering the pipeline.

   * **`Msg::Service(service::Msg)`**: Injects application-layer messages via the Service API. This tests the Northbound interface (Application Registration, ADU Send/Receive).

   * **`Msg::TickTimer(u64)`**: Advances the mock clock by a delta (triggering expiry/resend checks).

   * **`Msg::UpdateRoute(Vec<Updates>)`**: Injects a configuration or routing update event (e.g., adding/removing routes) to test dynamic reconfiguration.

4. **Invariant Check:** The BPA loop must process the event and return to an `await` state without panicking.

### 3.2 Coverage Scope

This target exercises the logic paths in `bpa/src/process.rs` (or equivalent core loop):

* **State Transitions:** `Pending` -> `Forwarded` -> `Deleted`.

* **Storage Locking:** concurrent access patterns (even in single-threaded Tokio, logical locks exist).

* **Routing Decisions:** Looking up routes for random EIDs.

* **Error Handling:** Processing "Store Failed" or "Transmit Failed" signals.

## 4. Vulnerability Classes & Mitigation

The fuzz target is specifically designed to uncover "Logic Bugs" rather than "Parsing Bugs":

| Vulnerability Class | Description | Mitigation Strategy Verified |
 | ----- | ----- | ----- |
| **Async Deadlock** | The loop waits on a channel that will never be signalled (e.g., waiting for Storage while holding the Ingress lock). | Verify the fuzzer completes iterations without hanging (timeout detection). |
| **State Corruption** | Attempting to delete a bundle that is already deleted, or forwarding a bundle that is not in storage. | Verify internal `expect` calls and `unwrap` safety on Option types. |
| **Lifecycle Errors** | Bundle expiry (LLR 1.1.33) failing when time jumps forward by `u64::MAX`. | Verify timestamp math uses saturating/checked arithmetic. |
| **Reassembly Loops** | Malformed fragment sequences causing infinite reassembly loops in `dispatch.rs`. | Verify loop iteration limits or state progress checks. |
| **Channel Saturation** | Flooding the ingress channel causes OOM or backpressure failure. | Verify graceful handling (`TrySendError`) of full channels. |

## 5. Execution & Configuration

### 5.1 Running the Fuzzer

Because the BPA involves the Tokio runtime, this fuzzer acts as a "Property-Based Test" running inside `libfuzzer`.

```bash
# Run for a set duration (Regression Mode)
cargo fuzz run bpa -- -max_total_time=3600 # 1 Hour

# Run indefinitely (Discovery Mode)
# Note: -j spawns multiple independent processes. Each process runs its own single-threaded Tokio runtime.
cargo fuzz run bpa -j $(nproc)
```

### 5.2 Configuration Notes

* **Runtime:** The target uses `tokio::runtime::Builder::new_current_thread()` to ensure deterministic execution on a single thread per process (avoiding true race conditions to focus on logic errors).
* **Sanitizers:** AddressSanitizer (ASAN) is less critical here than in parsers, but ThreadSanitizer (TSAN) can be useful if internal mutexes are used.

## 6. Pass/Fail Criteria

* **PASS:** Zero crashes (panics) and zero timeouts (>10s hang per iteration).
* **FAIL:**
  * **Panic:** Logic error (e.g., `Option::unwrap()` on `None`).
  * **Timeout:** Deadlock in the `select!` loop.

## 7. Corpus Management

* **Location:** `bpa/fuzz/corpus/bpa/`
* **Seed Data:**
  * Sequences of valid Bundle Ingress events.
  * Sequences triggering rapid expiry (Time += 100 years).
