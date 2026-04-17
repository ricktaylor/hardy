# Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Project** | Hardy DTN Router |
| **Date** | 2026-04-17 |
| **Version** | 1.2 |

## 1. Introduction

This report summarizes the test planning and execution status for the Hardy project. The testing strategy employs a modular architecture with coverage distributed across Unit, Component, Integration, and System levels.

**Overall status:**

* **Core Logic:** CBOR 68.2% line coverage (100% plan). BPv7 78.5% line coverage (100% plan). EID patterns 56.3% line coverage (85% plan, DTN glob matching known-broken).
* **BPA:** 65.4% line coverage, 55/59 plan scenarios (93%). 2 stubs remaining (queue selection/fallback). 11 fuzz targets across 5 crates.
* **Storage:** Generic integration suite covers trait-level CRUD, polling, and recovery for all backends. SQLite 70.9%, localdisk 77.1%, PostgreSQL and S3 via harness (14/14 and 4/4 plan).
* **Transport:** TCPCLv4 interop-verified with 7 independent implementations + 2 fuzz targets. All 10 LLRs verified.
* **gRPC Proxies:** Proto crate 31/31 plan tests, 80.4% line coverage.
* **TVR:** 77.2% line coverage, 137/137 unit tests, 10 system/component integration tests via grpcurl.
* **Infrastructure:** async 85.6%, ipn-legacy-filter 97.9%, OTEL 81.4% line coverage. All plan coverage 100%.
* **Interoperability:** 7 peer implementations passing (dtn7-rs, HDTN, DTNME, ud3tn, ION, ESA-BP, NASA cFS).

## 2. Coverage Summary

The full test plan inventory (32 plans across Unit, Component, Integration, Fuzz, and System levels) is maintained in the [Test Strategy](test_strategy.md) §2. All plans are Complete.

### 2.1 Crate-Level Coverage Reports

| Module | Report | Line Coverage | Plan Coverage |
| :--- | :--- | :--- | :--- |
| **cbor** | [`test_coverage_report.md`](../cbor/docs/test_coverage_report.md) | 68.2% (generic monomorphisation) | 38/38 (100%) |
| **bpv7** | [`test_coverage_report.md`](../bpv7/docs/test_coverage_report.md) | 78.5% | 21/21 (100%) |
| **eid-patterns** | [`test_coverage_report.md`](../eid-patterns/docs/test_coverage_report.md) | 56.3% (DTN glob broken) | 22/26 (85%) |
| **bpa** | [`test_coverage_report.md`](../bpa/docs/test_coverage_report.md) | 65.4% | 55/59 (93%) |
| **proto** | [`test_coverage_report.md`](../proto/docs/test_coverage_report.md) | 80.4% (generic monomorphisation) | 31/31 (100%) |
| **otel** | [`test_coverage_report.md`](../otel/docs/test_coverage_report.md) | 81.4% (99.6% `metrics_otel.rs`) | 26/26 (100%) |
| **tcpclv4** | [`test_coverage_report.md`](../tcpclv4/docs/test_coverage_report.md) | 25.0% | 10/10 (100%) |
| **tvr** | [`test_coverage_report.md`](../tvr/docs/test_coverage_report.md) | 77.2% | 137/137 (100%) |
| **async** | [`test_coverage_report.md`](../async/docs/test_coverage_report.md) | 85.6% | Not yet measured |
| **ipn-legacy-filter** | [`test_coverage_report.md`](../ipn-legacy-filter/docs/test_coverage_report.md) | 97.9% | 7/7 (100%) |
| **localdisk-storage** | [`test_coverage_report.md`](../localdisk-storage/docs/test_coverage_report.md) | 77.1% | 9/9 (100%) |
| **sqlite-storage** | [`test_coverage_report.md`](../sqlite-storage/docs/test_coverage_report.md) | 70.9% | 20/20 (100%) |
| **postgres-storage** | [`test_coverage_report.md`](../postgres-storage/docs/test_coverage_report.md) | Tested via storage harness | 14/14 (100%) |
| **s3-storage** | [`test_coverage_report.md`](../s3-storage/docs/test_coverage_report.md) | Tested via storage harness | 4/4 (100%) |
| **bpa-server** | [`test_coverage_report.md`](../bpa-server/docs/test_coverage_report.md) | 53.6% | 27/37 (73%) |
| **tcpclv4-server** | [`test_coverage_report.md`](../tcpclv4-server/docs/test_coverage_report.md) | 78.9% | Pending |
| **tools** | [`test_coverage_report.md`](../tools/docs/test_coverage_report.md) | CLI wrapper (verified via interop) | — |
| **echo-service** | [`test_coverage_report.md`](../echo-service/docs/test_coverage_report.md) | Verified via interop | — |
| **interop** | [`test_coverage_report.md`](../tests/interop/docs/test_coverage_report.md) | Shell-scripted tests | 6/11 (55%) |

## 3. Test Statistics

| Metric | Count |
| :--- | :--- |
| Workspace crates | 33 |
| `#[test]` functions | ~315 |
| Fuzz targets | 11 (cbor: 1, bpv7: 3, eid-patterns: 1, bpa: 4, tcpclv4: 2) |
| Test plan documents | 32 (all present) |
| PICS items mapped to tests | 49 (16 fully tested, 14 planned, 15 N/A or not implemented) |
| Interop peers | 7 passing (dtn7-rs, HDTN, DTNME, ud3tn, ION, ESA-BP, cFS) |

15 PICS items have test scenarios mapped in [PICS_Test_Mapping.md](PICS_Test_Mapping.md). BPA has 55/59 in-scope plan scenarios implemented (93%). 2 commented-out stubs remain in `bpa/src/cla/peers.rs` (queue selection/fallback — post-initial-phase scope).

## 4. Test Gaps

| Area | Gap | Notes |
| :--- | :--- | :--- |
| **eid-patterns** | DTN glob matching broken | 22/26 plan coverage (85%) |
| **bpa** | Queue selection/fallback stubs | 55/59 plan coverage (93%), 2 stubs remaining |
| **bpa-server** | System-level test gaps | 27/37 plan coverage (73%) |
| **interop** | IOP-06 (load), IOP-08/09 (admin), IOP-10 (BPSec), IOP-11 (fragmentation) | 6/11 plan coverage (55%) |
| **tcpclv4-server** | — | 78.9% line coverage measured |

For requirements-to-test mapping and implementation gaps, see the [Requirements Coverage Report](requirements_coverage_report.md).

## 5. Conclusion

Test plans are present for all crates (32 plans), consistent in format, and traceable to Low-Level Requirements. Tests are executed continuously via CI. Coverage highlights: 16 crates have line coverage measured (25%–98%), 11 fuzz targets across 5 crates, and interoperability verified across 7 peer implementations (20/20 at 0% loss). The primary test gaps are in bpa-server system scenarios and interop peer coverage.
