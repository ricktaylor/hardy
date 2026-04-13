# BPA Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Module** | `hardy-bpa` |
| **Standard** | RFC 9171 — Bundle Protocol Version 7 |
| **Test Plans** | [`UTP-BPA-01`](unit_test_plan.md) (Unit), [`PLAN-BPA-01`](component_test_plan.md) (Component) |
| **Date** | 2026-04-13 |

## 1. LLR Coverage Summary (Requirements Verification Matrix)

All LLRs assigned to this module pass (7 pass, 3 pass via bpv7, 1 N/A).

| LLR | Feature | Result | Test | Part 4 Ref |
| :--- | :--- | :--- | :--- | :--- |
| **1.1.30** | Rewriting rules for unknown blocks | Pass (bpv7) | `bpv7/parse.rs::unknown_block_discard` + CLI REWRITE-01. BPA delegates to parser | 1.2 |
| **1.1.31** | Non-canonical rewriting | Pass (bpv7) | `bpv7/parse.rs::non_canonical_rewriting`. BPA rfc9171 filter applies policy | 1.2 |
| **1.1.33** | Bundle Age for expiry calculation | Pass | `bundle/core.rs::test_age_fallback` (zero timestamp + age → creation time) | 1.2 |
| **1.1.34** | Hop Count processing | Pass (bpv7) | `bpv7/parse.rs::hop_count_extraction`. BPA enforces in `dispatch.rs:249` + increments in `forward.rs:106` | 1.2 |
| **2.1.1** | BPSec integrity/confidentiality | Pass (bpv7) | 16 unit tests in `bpv7/bpsec/rfc9173/test.rs` + 12 CLI tests | 2.3, 2.4 |
| **2.1.2** | BPSec target cleanup | Pass (bpv7) | `bpv7/test_bib_removal_and_readd`, `test_bcb_without_bib_removal` | 2.3 |
| **2.1.3** | Fragment + BPSec rejection | N/A | Sender constraint enforced by `bpv7/src/bpsec/signer.rs:75`. LLR to be corrected | 2.3 |
| **6.1.1** | EID pattern parsing | Pass (eid-patterns) | `str_tests::tests` — IPN + DTN parsing and matching | 6.6 |
| **6.1.9** | Route prioritisation | Pass | `rib/route.rs::test_action_precedence`, `test_route_entry_sort` | 6.6 |
| **6.1.10** | ECMP | Pass | `rib/find.rs::test_ecmp_hashing` | 6.6 |
| **7.1.3** | Configurable discard policy | Pass | `storage/bundle_mem.rs::test_eviction_policy_fifo`, `test_min_bundles_protection` | 7.1 |

## 2. Test Inventory

### Unit Tests (55 functions)

| Test Function | File | Plan Section | Scope |
| :--- | :--- | :--- | :--- |
| `test_age_fallback` | `src/bundle/core.rs` | §3.11 | Zero timestamp → creation time from age |
| `test_expiry_calculation` | `src/bundle/core.rs` | §3.11 | Expiry = creation + lifetime |
| `test_single_scheme_enforce` | `src/node_ids.rs` | §3.10 | Rejects conflicting IPN/DTN IDs |
| `test_invalid_types` | `src/node_ids.rs` | §3.10 | Rejects LocalNode |
| `test_admin_resolution_ipn` | `src/node_ids.rs` | §3.10 | 3-element IPN dest → `Eid::Ipn` admin; legacy 2-element → `Eid::LegacyIpn` admin |
| `test_admin_resolution_dtn` | `src/node_ids.rs` | §3.10 | DTN dest → DTN node EID; DTN-only fallback |
| `test_cache_ordering` | `src/storage/reaper.rs` | §3.9 | BTreeSet sorts by expiry time |
| `test_cache_saturation` | `src/storage/reaper.rs` | §3.9 | Sooner entry evicts latest when full |
| `test_cache_rejection` | `src/storage/reaper.rs` | §3.9 | Later entry rejected when full |
| `test_wakeup_trigger` | `src/storage/reaper.rs` | §3.9 | Wakeup on empty cache or new soonest |
| `test_flow_classification` | `src/policy/mod.rs` | §3.3 | NullPolicy classify → None |
| `test_queue_bounds` | `src/policy/mod.rs` | §3.3 | Classify never exceeds queue_count |
| `test_quota_enforcement` | `src/storage/store.rs` | §3.6 | Duplicate bundle rejected |
| `test_double_delete` | `src/storage/store.rs` | §3.6 | Idempotent deletion |
| `test_transaction_rollback` | `src/storage/store.rs` | §3.6 | Data cleanup on metadata duplicate |
| `test_eviction_policy_fifo` | `src/storage/bundle_mem.rs` | §3.6 | LRU eviction on capacity overflow |
| `test_eviction_policy_priority` | `src/storage/bundle_mem.rs` | §3.6 | FIFO eviction by insertion order (`load` uses `peek`, no promotion) |
| `test_min_bundles_protection` | `src/storage/bundle_mem.rs` | §3.6 | min_bundles overrides byte quota |
| `test_large_quota_config` | `src/storage/bundle_mem.rs` | §3.6 | NonZeroUsize handles >1TB capacity |
| `test_fast_path_saturation` | `src/storage/channel.rs` | §3.7 | Fill channel → Draining state |
| `test_congestion_signal` | `src/storage/channel.rs` | §3.7 | Send while Draining → Congested |
| `test_hysteresis_recovery` | `src/storage/channel.rs` | §3.7 | Drain + tombstone → poller re-opens to Open |
| `test_lazy_expiry` | `src/storage/channel.rs` | §3.7 | Expired bundle on fast path doesn't crash channel |
| `test_close_safety` | `src/storage/channel.rs` | §3.7 | Send after close returns SendError |
| `test_drop_to_storage_integrity` | `src/storage/channel.rs` | §3.7 | Overflow bundle arrives via poller slow path |
| `test_hybrid_duplication` | `src/storage/channel.rs` | §3.7 | All bundles arrive with prompt tombstoning (at-least-once) |
| `test_ordering_preservation` | `src/storage/channel.rs` | §3.7 | Both bundles arrive (fast path=FIFO, slow path=`received_at` ASC) |
| `test_status_consistency` | `src/storage/channel.rs` | §3.7 | Received bundle has correct ForwardPending status |
| `test_zombie_task_leak` | `src/storage/channel.rs` | §3.7 | Close sentinel (None) arrives after close |
| `reassemble_rejects_incomplete_coverage` | `src/storage/adu_reassembly.rs` | §3.5 | Incomplete fragment coverage |
| `reassemble_rejects_fragment_beyond_bounds` | `src/storage/adu_reassembly.rs` | §3.5 | Fragment exceeds ADU length |
| `reassemble_rejects_missing_first_fragment` | `src/storage/adu_reassembly.rs` | §3.5 | No offset-0 fragment |
| `reassemble_rejects_adu_length_mismatch` | `src/storage/adu_reassembly.rs` | §3.5 | Conflicting total lengths |
| `reassemble_basic_happy_path` | `src/storage/adu_reassembly.rs` | §3.5 | Two fragments → complete bundle via Builder + Editor |
| `test_duplicate_reg` | `src/services/registry.rs` | §3.4 | Duplicate explicit IPN service number rejected |
| `test_cleanup` | `src/services/registry.rs` | §3.4 | Service ID freed after sink unregister, re-registration succeeds |
| `test_address_parsing` | `src/cla/mod.rs` | §3.8 | ClaAddress round-trip (TCP, IPv6, Private) |
| `test_duplicate_registration` | `src/cla/registry.rs` | §3.8 | Duplicate CLA name rejected |
| `test_peer_lifecycle` | `src/cla/registry.rs` | §3.8 | add_peer/remove_peer + double-remove |
| `test_cascading_cleanup` | `src/cla/registry.rs` | §3.8 | CLA unregister removes all peers, name freed |
| `test_exact_match` | `src/rib/find.rs` | §3.2 | Exact EID route lookup |
| `test_default_route` | `src/rib/find.rs` | §3.2 | Catch-all Via route |
| `test_no_route` | `src/rib/find.rs` | §3.2 | No routes → None |
| `test_recursion_loop` | `src/rib/find.rs` | §3.2 | Circular routes → Drop |
| `test_reflection` | `src/rib/find.rs` | §3.2 | Reflect via previous node |
| `test_reflection_no_double` | `src/rib/find.rs` | §3.2 | Prevent double reflection |
| `test_ecmp_hashing` | `src/rib/find.rs` | §3.2 | Deterministic ECMP peer selection |
| `test_action_precedence` | `src/rib/route.rs` | §3.2 | Drop < Reflect < Via ordering |
| `test_route_entry_sort` | `src/rib/route.rs` | §3.2 | BTreeSet ordering |
| `test_entry_source_tiebreak` | `src/rib/route.rs` | §3.2 | Source name ordering |
| `test_entry_dedup` | `src/rib/route.rs` | §3.2 | Duplicate rejection |
| `test_local_action_sort` | `src/rib/local.rs` | §3.2 | Local action ordering |
| `test_implicit_routes` | `src/rib/local.rs` | §3.2 | Default routes on startup |
| `test_local_ephemeral` | `src/rib/local.rs` | §3.2 | Known-local, no service → Drop |
| `test_impacted_subsets` | `src/rib/mod.rs` | §3.2 | Priority-based route insertion |

### Fuzz Tests

| Target | File | Status |
| :--- | :--- | :--- |
| `bpa` | `fuzz/fuzz_targets/bpa.rs` | Implemented — random events (bundle received, route updates) |

### Pipeline Integration Tests

5 tests in `tests/pipeline.rs` — end-to-end bundle processing through the BPA with inline mock CLAs and services.

| Test Function | Plan Ref | Scope |
| :--- | :--- | :--- |
| `app_to_cla_routing` | INT-BPA-01 | Application sends bundle via `ApplicationSink`, BPA routes to CLA peer |
| `echo_round_trip` | INT-BPA-02 | CLA dispatches to echo service, reply forwarded back with swapped endpoints |
| `local_delivery` | — | CLA dispatches to local application, payload delivered via `on_receive` |
| `throughput` | PERF-01 | 1000 bundles, concurrent dispatch+receive, asserts >1000 bundles/sec |
| `forwarding_latency` | PERF-LAT-01 | 100 bundles, per-bundle latency via `TimedCla`, reports P50/P95/P99 |

### Criterion Benchmark

`benches/bundle_bench.rs` — single-bundle forwarding throughput via criterion.

### Performance Results (2026-04-13)

Test machine: Intel Xeon @ 2.20GHz, 4 cores, 15 GB RAM, Debian 12, x86_64. Tokio multi-thread (4 workers). In-memory storage, no I/O.

| Measurement | Method | Result |
| :--- | :--- | :--- |
| Throughput | `tests/pipeline.rs::throughput` | 4,078 bundles/sec (REQ-13 target: >1,000) |
| Latency | `tests/pipeline.rs::forwarding_latency` | P50=536µs, P95=1.19ms, P99=1.31ms |
| Throughput (criterion) | `benches/bundle_bench.rs` | 8,026 bundles/sec (125µs median, 100 samples) |

Pipeline test results are from the `test` profile (debug, unoptimised); criterion results are from the `bench` profile (optimised). Results are machine-dependent; performance tests print system info via `--nocapture`.

### Interop Tests

Covered by interop test suite (`tests/interop/`). All 7 implementations passing 20/20 at 0% loss.

## 3. Coverage vs Plan

### 3.1 Unit Test Plan (UTP-BPA-01)

| Section | Scenario | Planned | Implemented | Status |
| :--- | :--- | :--- | :--- | :--- |
| §3.1 Status Reports | Route Missing, TTL Expired | 2 | 0 | **Covered by fuzz** — `dispatcher/report.rs` at 98% fuzz coverage, `admin.rs` at 85% |
| §3.2 Routing Table | 15 scenarios | 15 | 15 | **Complete** |
| §3.3 Egress Policy | Flow Classification, Queue Bounds | 2 | 2 | **Complete** |
| §3.4 Service Registry | Duplicate Reg, Cleanup | 2 | 2 | **Complete** — via `Bpa::builder()` |
| §3.5 Reassembly | 5 scenarios | 5 | 5 | **Complete** |
| §3.6 Storage / Quotas | 7 scenarios | 7 | 7 | **Complete** |
| §3.7 Channel State Machine | 10 scenarios | 10 | 10 | **Complete** — `multi_thread` runtime + `pub(crate)` state accessor |
| §3.8 CLA Registry | 6 scenarios | 6 | 4 | Queue Selection/Fallback remaining — needs multi-queue policy mock |
| §3.9 Reaper | 4 scenarios | 4 | 4 | **Complete** |
| §3.10 Node IDs | 4 scenarios | 4 | 4 | **Complete** |
| §3.11 Bundle Time Math | 2 scenarios | 2 | 2 | **Complete** |
| §3.12 BPSec Policy | 2 scenarios | — | — | **Delegated to bpv7** (Rev 3) — Fragment Security: `signer.rs:75`. Target Cleanup: `test_bib_removal_and_readd` |
| §3.13 Canonicalization | 2 scenarios | — | — | **Delegated to bpv7** (Rev 3) — `parse.rs::unknown_block_discard` + CLI REWRITE-01 |
| **Total** | | **59** | **55** | **93%** |

4 scenarios delegated to bpv7 test suite (§3.12: 2, §3.13: 2). 5 remaining in BPA scope.

### 3.2 Fuzz Test Plan (FUZZ-BPA-01)

| Target | Planned Msg Variants | Implemented | Status |
| :--- | :--- | :--- | :--- |
| `bpa` | `Cla(RandomBundle)` | Yes | Structured random bundles via CLA |
| `bpa` | `ClaBytes(Vec<u8>)` | Yes | Raw bytes via CLA (parser + pipeline) |
| `bpa` | `Service(Msg)` | Yes | Application API (send via service) |
| `bpa` | `TickTimer(u64)` | No | Cannot test expiry paths |
| `bpa` | `UpdateRoute(Vec<Updates>)` | No | Cannot test dynamic route changes |
| **Total** | **5** | **3** | **60%** |

Fuzz coverage (61,673 corpus inputs, 2026-04-13):

```
  lines......: 49.7% (5006 of 10068 lines)
```

Key BPA files exercised by fuzz (not significantly covered by unit/pipeline tests):

| File | Fuzz | Unit+Pipeline | Notes |
| :--- | :--- | :--- | :--- |
| `dispatcher/report.rs` | 98% | 11% | Status report generation — fuzz effectively covers §3.1 |
| `dispatcher/admin.rs` | 85% | 0% | Administrative record processing |
| `dispatcher/dispatch.rs` | 81% | 29% | Core dispatch pipeline |
| `dispatcher/forward.rs` | 73% | 37% | Bundle forwarding |
| `dispatcher/local.rs` | 72% | 39% | Local delivery |
| `dispatcher/mod.rs` | 90% | 70% | Pipeline orchestration |
| `storage/store.rs` | 67% | 46% | Store operations |

### 3.3 Component Test Plan (PLAN-BPA-01)

| Section | Item | Status |
| :--- | :--- | :--- |
| §4A | App-to-CLA Routing | **Implemented** — `tests/pipeline.rs::app_to_cla_routing` |
| §4B | Echo Round-Trip | **Implemented** — `tests/pipeline.rs::echo_round_trip` |
| §4B+ | Local Delivery | **Implemented** — `tests/pipeline.rs::local_delivery` |
| §4C | Fragment Reassembly | Not implemented as pipeline test (unit test covers reassembly logic) |
| §5 | Throughput (PERF-01) | **Implemented** — `tests/pipeline.rs::throughput` (5,130 bundles/sec) + `benches/bundle_bench.rs` (criterion: 8,026/sec). REQ-13 target: >1,000/sec |
| §5.1 | Latency (PERF-LAT-01) | **Implemented** — `tests/pipeline.rs::forwarding_latency` (P50=536µs, P95=1.19ms, P99=1.31ms) + criterion (125µs median) |
| §5.2 | BPSec Performance (PERF-SEC-01 to SEC-03) | Not implemented |

## 4. Line Coverage

```
cargo llvm-cov test --package hardy-bpa --lcov --output-path lcov.info --html
```

Results (2026-04-13):

```
  lines......: 57.0% (3125 of 5478 lines)
  functions..: 32.2% (591 of 1834 functions)
```

Line coverage is for production code only (test modules excluded). Function count is inflated by generic monomorphisation. The pipeline integration tests (`tests/pipeline.rs`) contributed a 10 percentage point increase by exercising the dispatcher pipeline end-to-end.

Per-file breakdown (from HTML report):

| File | Covered | Total | Coverage | Notes |
| :--- | :--- | :--- | :--- | :--- |
| `bundle/metadata.rs` | 17 | 17 | 100% | Complete |
| `filters/mod.rs` | 7 | 7 | 100% | Complete |
| `cla/egress_queue.rs` | 23 | 25 | 92% | Exercised via pipeline tests |
| `policy/mod.rs` | 17 | 18 | 94% | Complete |
| `node_ids.rs` | 126 | 135 | 93% | Complete |
| `bundle/core.rs` | 71 | 78 | 91% | Complete |
| `storage/bundle_mem.rs` | 79 | 88 | 90% | Eviction + config tests |
| `rib/mod.rs` | 100 | 113 | 89% | Complete |
| `cla/mod.rs` | 49 | 56 | 88% | Address parsing |
| `rib/find.rs` | 251 | 287 | 87% | Route lookup (15 tests) |
| `policy/null_policy.rs` | 13 | 15 | 87% | Classify + controller |
| `filters/rfc9171.rs` | 17 | 20 | 85% | Exercised via pipeline tests |
| `cla/peers.rs` | 109 | 129 | 85% | Lifecycle + pipeline tests |
| `storage/channel.rs` | 385 | 539 | 71% | 10 state machine tests |
| `dispatcher/mod.rs` | 107 | 153 | 70% | Dispatcher setup + pipeline tests |
| `otel_metrics.rs` | 47 | 70 | 67% | Metric init |
| `builder.rs` | 72 | 109 | 66% | Exercised by `Bpa::builder()` tests |
| `rib/local.rs` | 151 | 231 | 65% | Local routing + implicit routes |
| `cla/registry.rs` | 230 | 380 | 61% | Registry tests + pipeline tests |
| `bpa.rs` | 41 | 68 | 60% | Registration API |
| `filters/registry.rs` | 53 | 106 | 50% | Exercised via pipeline tests |
| `storage/adu_reassembly.rs` | 263 | 538 | 49% | 5 tests + reassembly pipeline |
| `storage/reaper.rs` | 101 | 213 | 47% | Cache tests (async reaper untested) |
| `storage/store.rs` | 208 | 454 | 46% | Store orchestration tests |
| `keys/registry.rs` | 17 | 36 | 47% | Exercised via pipeline tests |
| `filters/filter.rs` | 106 | 245 | 43% | Exercised via pipeline tests |
| `dispatcher/local.rs` | 122 | 316 | 39% | Exercised via pipeline local delivery |
| `dispatcher/forward.rs` | 56 | 152 | 37% | Exercised via pipeline forwarding |
| `services/registry.rs` | 178 | 482 | 37% | 2 lifecycle tests + pipeline tests |
| `storage/metadata_mem.rs` | 62 | 179 | 35% | Exercised indirectly via Store tests |
| `dispatcher/dispatch.rs` | 111 | 387 | 29% | Exercised via pipeline tests |
| `rib/route.rs` | 55 | 268 | 21% | Route entry tests (generic impls inflate total) |
| `rib/agent.rs` | 12 | 90 | 13% | |
| `dispatcher/report.rs` | 27 | 236 | 11% | Partially exercised via pipeline |
| `dispatcher/admin.rs` | 0 | 99 | 0% | Admin records not exercised |
| `dispatcher/reassemble.rs` | 0 | 49 | 0% | Reassembly pipeline not exercised (unit test covers logic) |
| `dispatcher/restart.rs` | 0 | 256 | 0% | Recovery not exercised |
| `storage/recover.rs` | 0 | 142 | 0% | Recovery not exercised |
| `routes.rs` | 0 | 19 | 0% | Trait definitions only |

**Note:** This covers unit tests only. The fuzz harness (`bpa/fuzz/`) and interop tests exercise the dispatcher pipeline code that shows 0% here.

## 5. Test Infrastructure

Tests use two approaches depending on what they exercise:

**Direct construction** for isolated logic (Reaper cache, Node IDs, Policy, Storage quotas, Bundle time math):
in-memory `Store` via `Store::new()` with `MetadataMemStorage` + `BundleMemStorage`.

**`Bpa::builder()` pattern** for registry lifecycle and channel state machine:

```rust
let bpa = Bpa::builder().status_reports(true).build();
bpa.start(false);
// register via BpaRegistration trait
bpa.shutdown().await;
```

Inline mock types (`TestApp`, `TestCla`) implement the `Application`/`Cla` traits with `spin::Once<Box<dyn Sink>>` storage. The fuzz harness (`bpa/fuzz/src/`) has additional mocks (`NullCla`, `PipeService`) for more complex scenarios.

**Channel tests** require `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` because the background poller runs as a spawned task. A `pub(crate)` `Sender::state()` accessor exposes `ChannelState` for assertions. Tests tombstone received bundles to simulate dispatcher processing (the channel provides at-least-once delivery; the consumer is responsible for deduplication via status changes).

## 6. Key Gaps

| Area | Gap | Severity | Action |
| :--- | :--- | :--- | :--- |
| Status report generation | 0/2 unit tests | Low | Deferred — exercised by fuzz harness; dedicated tests need full pipeline |
| CLA queue selection | 2/6 remaining | Low | Post-de-risk — multi-queue not in scope |

## 7. Conclusion

The BPA crate has **complete LLR coverage** (7 pass, 3 pass via bpv7, 1 N/A; Part 4 refs 1.2, 2.3, 2.4, 6.6, 7.1) with 55 unit test functions covering 93% of in-scope plan scenarios (55/59), 5 pipeline integration tests, a criterion benchmark, 57.0% unit+pipeline line coverage (3125/5478), and 49.7% fuzz line coverage (5006/10068) from 61,673 corpus inputs. The fuzz coverage is highly complementary — it achieves 98% on `dispatcher/report.rs` and 85% on `dispatcher/admin.rs`, areas untouched by unit tests. Four scenarios (§3.12 BPSec Policy, §3.13 Canonicalization) are delegated to the bpv7 test suite. One fuzz target exercises the full pipeline with random events. Integration-level coverage is provided by interoperability testing with 7 independent Bundle Protocol implementations.

The pipeline tests (`tests/pipeline.rs`) exercise the dispatcher end-to-end: app-to-CLA routing, echo round-trip, local delivery, throughput (5,130 bundles/sec, REQ-13 target >1,000), and forwarding latency (P50=536µs, P95=1.19ms, P99=1.31ms). The criterion benchmark (`benches/bundle_bench.rs`) provides statistically rigorous throughput measurement at ~8K bundles/sec with in-memory storage. The `Bpa::builder()` pattern proved effective for both unit tests and integration tests with inline mock types.
