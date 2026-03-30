# Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Project** | Hardy (Cloud-based DTN Router) |
| **Date** | 2026-03-30 |
| **Status** | READY FOR REVIEW |

## 1. Executive Summary

This report summarizes the test planning and execution status for the Hardy project. The testing strategy employs a modular architecture with coverage distributed across Unit, Component, Integration, and System levels.

**Overall Status:**

* **Core Logic:** High coverage (Unit + Fuzzing). CBOR crate fully covered (all LLRs verified, ~100% effective coverage). BPv7 at 77.1% line coverage with 100% test plan coverage (21/21 scenarios). EID patterns at 56.3% line coverage (~95% effective for IPN, DTN glob matching known-broken).
* **Storage:** High coverage (Generic Integration Suite in `tests/storage` covers trait-level CRUD, polling, and recovery for all backends). Backend-specific gaps: SQLite migration/concurrency/corruption; localdisk dirty-directory cleanup/filesystem structure.
* **Transport:** High coverage (TCPCLv4 interop tests with 4 independent implementations + 2 fuzz targets). RFC 9174 compliance matrix complete (all 10 LLRs verified).
* **System:** Moderate coverage (Basic End-to-End & Interop defined). Interop tests for dtn7-rs, HDTN, DTNME, ud3tn, and hardy-to-hardy passing.
* **BPA:** Low unit test coverage (28% — only RIB tested, 36 stubs with `todo!()`). Covered at system level by interop tests and fuzz harness.

## 2. Test Plan Inventory

| Module | Type | Plan ID | Requirements Covered | Status |
| :--- | :--- | :--- | :--- | :--- |
| **cbor** | Unit | [`UTP-CBOR-01`](../cbor/docs/unit_test_plan.md) | [REQ-1](requirements.md#req-1-full-compliance-with-rfc9171) (RFC 8949) | **Complete** |
| **bpv7** | Unit | [`UTP-BPV7-01`](../bpv7/docs/unit_test_plan.md) | [REQ-1](requirements.md#req-1-full-compliance-with-rfc9171) (RFC 9171) | **Complete** |
| **bpv7** | Component | [`COMP-BPV7-CLI-01`](../bpv7/docs/component_test_plan.md) | [REQ-1](requirements.md#req-1-full-compliance-with-rfc9171), [REQ-2](requirements.md#req-2-support-for-bpsec-rfc9172-and-default-security-contexts-rfc9173) (BPSec) | **Complete** |
| **bpsec** | Unit | [`UTP-BPSEC-01`](../bpv7/docs/unit_test_plan_bpsec.md) | [REQ-2](requirements.md#req-2-support-for-bpsec-rfc9172-and-default-security-contexts-rfc9173) (RFC 9172/3) | **Complete** |
| **eid-patterns** | Unit | [`UTP-PAT-01`](../eid-patterns/docs/unit_test_plan.md) | [REQ-1](requirements.md#req-1-full-compliance-with-rfc9171) (EID Pattern Matching) | **Complete** |
| **bpa** | Unit | [`UTP-BPA-01`](../bpa/docs/unit_test_plan.md) | [REQ-6](requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth) (Routing), [REQ-7](requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage) (Storage) | **Complete** |
| **bpa** | Integration | [`PLAN-BPA-01`](../bpa/docs/component_test_plan.md) | [REQ-13](requirements.md#req-13-performance) (Perf), Pipeline | **Complete** |
| **bpa** | Trait | [`PLAN-CLA-01`](../bpa/docs/cla_integration_test_plan.md) | [REQ-3](requirements.md#req-3-full-compliance-with-rfc9174), [REQ-6](requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth) | **Complete** |
| **bpa** | Trait | [`PLAN-SVC-01`](../bpa/docs/service_integration_test_plan.md) | [REQ-18](requirements.md#req-18-grpc-based-internal-apis-for-component-communication) | **Complete** |
| **bpa** | Trait | [`PLAN-STORE-01`](../bpa/docs/storage_integration_test_plan.md) | [REQ-7](requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [REQ-8](requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage), [REQ-9](requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage) | **Complete** |
| **tcpclv4** | Component | [`PLAN-TCPCL-01`](../tcpclv4/docs/component_test_plan.md) | [REQ-3](requirements.md#req-3-full-compliance-with-rfc9174) (RFC 9174) | **Complete** |
| **tcpclv4-server** | System | [`PLAN-TCPCL-SERVER-01`](../tcpclv4-server/docs/test_plan.md) | [REQ-3](requirements.md#req-3-full-compliance-with-rfc9174), [REQ-15](requirements.md#req-15-independent-component-packaging), [REQ-16](requirements.md#req-16-kubernetes-packaging) | **Complete** |
| **localdisk-storage** | Component | [`PLAN-LD-01`](../localdisk-storage/docs/test_plan.md) | [REQ-7](requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage) (Filesystem) | **Complete** |
| **sqlite-storage** | Component | [`PLAN-SQLITE-01`](../sqlite-storage/docs/test_plan.md) | [REQ-7](requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage) (Metadata) | **Complete** |
| **proto** | Component | [`COMP-GRPC-CLIENT-01`](../proto/docs/component_test_plan.md) | [REQ-18](requirements.md#req-18-grpc-based-internal-apis-for-component-communication) (API) | **Complete** |
| **bpa-server** | System | [`PLAN-SERVER-01`](../bpa-server/docs/test_plan.md) | [REQ-14](requirements.md#req-14-reliability), [REQ-15](requirements.md#req-15-independent-component-packaging), [REQ-19](requirements.md#req-19-a-well-featured-suite-of-management-and-monitoring-tools) | **Complete** |
| **System** | Interop | [`PLAN-INTEROP-01`](interop_test_plan.md) | [REQ-20](requirements.md#req-20-interoperability-with-reference-implementations) (ION/DTNME) | **Complete** |

### 2.1 Fuzz Test Plans

| Module | Plan ID | Scope | Status |
| :--- | :--- | :--- | :--- |
| **cbor** | [`FUZZ-CBOR-01`](../cbor/docs/fuzz_test_plan.md) | Decoder robustness (Stack/OOM) | **Complete** |
| **bpv7** | [`FUZZ-BPV7-01`](../bpv7/docs/fuzz_test_plan.md) | Bundle parsing, EID string/CBOR parsing | **Complete** |
| **eid-patterns** | [`FUZZ-PAT-01`](../eid-patterns/docs/fuzz_test_plan.md) | Pattern DSL parser robustness | **Complete** |
| **bpa** | [`FUZZ-BPA-01`](../bpa/docs/fuzz_test_plan.md) | Async pipeline stability and deadlocks | **Complete** |
| **tcpclv4** | [`FUZZ-TCPCL-01`](../tcpclv4/docs/fuzz_test_plan.md) | Protocol stream parsing (passive + active) | **Complete** |

### 2.2 Crate-Level Coverage Reports

| Module | Report | Line Coverage | Plan Coverage |
| :--- | :--- | :--- | :--- |
| **cbor** | [`test_coverage_report.md`](../cbor/docs/test_coverage_report.md) | 68.2% (generic monomorphisation) | 38/38 (100%) |
| **bpv7** | [`test_coverage_report.md`](../bpv7/docs/test_coverage_report.md) | 78.2% | 21/21 (100%) |
| **eid-patterns** | [`test_coverage_report.md`](../eid-patterns/docs/test_coverage_report.md) | 56.3% (DTN glob broken) | 22/26 (85%) |
| **tcpclv4** | [`test_coverage_report.md`](../tcpclv4/docs/test_coverage_report.md) | N/A (interop-verified) | 10/10 LLRs (100%) |

## 3. Test Statistics

| Metric | Count |
| :--- | :--- |
| Workspace crates | 33 |
| `#[test]` functions | ~250 |
| Fuzz targets | 11 (cbor: 1, bpv7: 3, eid-patterns: 1, bpa: 4, tcpclv4: 2) |
| Test plan documents | 22 (all present) |
| PICS items mapped to tests | 49 (16 fully tested, 14 planned, 15 N/A or not implemented) |
| Interop peers | 4 passing on main (dtn7-rs, HDTN, DTNME, ud3tn), 3 on branches (ION, ESA-BP, cFS) |

15 PICS items have test scenarios mapped in [PICS_Test_Mapping.md](PICS_Test_Mapping.md) that reference BPA unit test plan stubs. 36 commented-out test stubs with `todo!()` remain across `bpa/src/`. These need to be implemented to achieve full PICS verification coverage.

## 4. Implementation Gaps

### 4.1 Not Implemented

| Feature | Requirement | Test Status |
| :--- | :--- | :--- |
| **UDP Convergence Layer** | [REQ-4](requirements.md#req-4-alignment-with-on-going-dtn-standardisation) | **Missing** (Not Implemented; [UDPCLv2](https://datatracker.ietf.org/doc/draft-ietf-dtn-udpcl/) planned) |
| **QUIC Convergence Layer** | [REQ-5](requirements.md#req-5-experimental-support-for-quic) | **Missing** (Not Implemented; [QUBICLE](https://datatracker.ietf.org/doc/draft-ek-dtn-qubicle/) planned) |
| **Guaranteed Delivery** | [REQ-4](requirements.md#req-4-alignment-with-on-going-dtn-standardisation) | **Missing** (Not implementing BPv6-style Custody Transfer; QoS + CBSR approach TBD) |
| **CBSR** | [REQ-4](requirements.md#req-4-alignment-with-on-going-dtn-standardisation) | **Missing** (Compressed Bundle Status Reporting; design doc exists) |
| **DynamoDB Metadata** | [REQ-10](requirements.md#req-10-support-for-amazon-dynamodb-for-bundle-metadata-storage) | **Missing** (Not Implemented) |
| **Azure Blob Storage** | [REQ-11](requirements.md#req-11-support-for-azure-blob-storage-for-bundle-storage) | **Missing** (Not Implemented) |
| **Azure SQL Metadata** | [REQ-12](requirements.md#req-12-support-for-azure-sql-for-bundle-metadata-storage) | **Missing** (Not Implemented) |
| **Helm Charts** | [REQ-16](requirements.md#req-16-kubernetes-packaging) | **Missing** (Defined in [`PLAN-SERVER-01`](../bpa-server/docs/test_plan.md) but not implemented) |
| **TVR Agent** | [REQ-6](requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth) | **Missing** (Scaffolding on feature branch; not on main) |
| **BP Tools (perf, send, trace)** | [REQ-19](requirements.md#req-19-a-well-featured-suite-of-management-and-monitoring-tools) (19.2.1, 19.2.3) | **Missing** (Only `bp ping` implemented) |

### 4.2 Implemented (with test gaps)

| Feature | Requirement | Test Status |
| :--- | :--- | :--- |
| **PostgreSQL Storage** | [REQ-8](requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage) | **Implemented** (Generic Suite covers trait-level; backend-specific migration/concurrency tests pending) |
| **S3 Storage** | [REQ-9](requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage) | **Implemented** (Generic Suite covers trait-level) |
| **OCI Packaging** | [REQ-15](requirements.md#req-15-independent-component-packaging) | **Partial** (Dockerfile + CI workflow exist; no published registry images) |
| **BPSec Key Providers** | [REQ-2](requirements.md#req-2-support-for-bpsec-rfc9172-and-default-security-contexts-rfc9173) | **Partial** (`KeyProvider` trait + Registry exist; no concrete providers) |
| **TCPCLv4 mTLS** | [REQ-3](requirements.md#req-3-full-compliance-with-rfc9174) | **Partial** (TLS supported; mutual TLS has TODO markers) |
| **BPA Unit Tests** | [REQ-1](requirements.md#req-1-full-compliance-with-rfc9171), [REQ-6](requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth) | **Partial** (15/53 plan scenarios implemented, 36 stubs with `todo!()`) |

### 4.3 PICS Compliance Gap

| PICS Item | Feature | Status | Impact |
| :--- | :--- | :--- | :--- |
| **28** | BP Managed Information (Annex C) | M / **N** | Only mandatory PICS item not implemented. CBSR-based approach planned instead. See [PICS_Test_Mapping.md](PICS_Test_Mapping.md) §4.1. |

## 5. Conclusion

The project has a comprehensive verification strategy for all implemented features. All 22 test plan documents are present and substantive (55–201 lines each). The test plans are consistent in format and traceable to the Low-Level Requirements (LLR). Tests are executed continuously via CI (`rust.yml`). The CBOR and BPv7 crates have complete LLR coverage with crate-level coverage reports. TCPCLv4 compliance is verified through interop testing with 3 independent implementations plus 2 fuzz targets. The primary test debt is in the BPA crate (36 `todo!()` stubs, 28% plan coverage) and the proto crate (0 tests implemented despite a complete test plan).
