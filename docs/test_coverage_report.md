# Test Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Project** | Hardy (Cloud-based DTN Router) |
| **Date** | 2026-01-19 |
| **Status** | READY FOR REVIEW |

## 1. Executive Summary

This report summarizes the test planning status for the Hardy project. The testing strategy employs a modular architecture with coverage distributed across Unit, Component, Integration, and System levels.

**Overall Status:**

* **Core Logic:** High coverage (Unit + Fuzzing).
* **Storage:** High coverage (Generic Integration Suite).
* **Transport:** High coverage (TCPCLv4 Component Tests).
* **System:** Moderate coverage (Basic End-to-End & Interop defined).

## 2. Test Plan Inventory

| Module | Type | Plan ID | Requirements Covered | Status |
| :--- | :--- | :--- | :--- | :--- |
| **cbor** | Unit | [`UTP-CBOR-01`](../cbor/docs/unit_test_plan.md) | [REQ-1](requirements.md#req-1-full-compliance-with-rfc9171) (RFC 8949) | **Complete** |
| **bpv7** | Unit | [`UTP-BPV7-01`](../bpv7/docs/unit_test_plan.md) | [REQ-1](requirements.md#req-1-full-compliance-with-rfc9171) (RFC 9171) | **Complete** |
| **bpv7** | Component | [`COMP-BPV7-CLI-01`](../bpv7/docs/component_test_plan.md) | [REQ-1](requirements.md#req-1-full-compliance-with-rfc9171), [REQ-2](requirements.md#req-2-support-for-bpsec-rfc9172-and-default-security-contexts-rfc9173) (BPSec) | **Complete** |
| **bpa** | Unit | [`UTP-BPA-01`](../bpa/src/unit_test_plan.md) | [REQ-6](requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth) (Routing), [REQ-7](requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage) (Storage) | **Complete** |
| **bpa** | Integration | [`PLAN-BPA-01`](../bpa/src/component_test_plan.md) | [REQ-13](requirements.md#req-13-performance) (Perf), Pipeline | **Complete** |
| **bpa** | Trait | [`PLAN-CLA-01`](../bpa/docs/cla_integration_test_plan.md) | [REQ-3](requirements.md#req-3-full-compliance-with-rfc9174), [REQ-6](requirements.md#req-6-time-variant-routing-api-to-allow-real-time-configuration-of-contacts-and-bandwidth) | **Complete** |
| **bpa** | Trait | [`PLAN-SVC-01`](../bpa/docs/service_integration_test_plan.md) | [REQ-18](requirements.md#req-18-grpc-based-internal-apis-for-component-communication) | **Complete** |
| **bpa** | Trait | [`PLAN-STORE-01`](../bpa/docs/storage_integration_test_plan.md) | [REQ-7](requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage), [REQ-8](requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage), [REQ-9](requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage) | **Complete** |
| **tcpclv4** | Component | [`PLAN-TCPCL-01`](../tcpclv4/docs/component_test_plan.md) | [REQ-3](requirements.md#req-3-full-compliance-with-rfc9174) (RFC 9174) | **Complete** |
| **tcpclv4-server** | System | [`PLAN-TCPCL-SERVER-01`](../tcpclv4-server/src/test_plan.md) | [REQ-3](requirements.md#req-3-full-compliance-with-rfc9174), [REQ-15](requirements.md#req-15-independent-component-packaging), [REQ-16](requirements.md#req-16-kubernetes-packaging) | **Complete** |
| **localdisk-storage** | Component | [`PLAN-LD-01`](../localdisk-storage/docs/test_plan.md) | [REQ-7](requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage) (Filesystem) | **Complete** |
| **sqlite-storage** | Component | [`PLAN-SQLITE-01`](../sqlite-storage/docs/test_plan.md) | [REQ-7](requirements.md#req-7-support-for-local-filesystem-for-bundle-and-metadata-storage) (Metadata) | **Complete** |
| **proto** | Component | [`COMP-GRPC-CLIENT-01`](../proto/docs/component_test_plan.md) | [REQ-18](requirements.md#req-18-grpc-based-internal-apis-for-component-communication) (API) | **Complete** |
| **bpa-server** | System | [`PLAN-SERVER-01`](../bpa-server/docs/test_plan.md) | [REQ-14](requirements.md#req-14-reliability), [REQ-15](requirements.md#req-15-independent-component-packaging), [REQ-19](requirements.md#req-19-a-well-featured-suite-of-management-and-monitoring-tools) | **Complete** |
| **System** | Interop | [`PLAN-INTEROP-01`](interop_test_plan.md) | [REQ-20](requirements.md#req-20-interoperability-with-reference-implementations) (ION/DTNME) | **Complete** |

## 3. Implementation Gaps

The following areas have defined requirements but lack implemented code or specific test plans (as noted in `missing_features.md`).

| Feature | Requirement | Test Status |
| :--- | :--- | :--- |
| **UDP Convergence Layer** | [REQ-4](requirements.md#req-4-alignment-with-on-going-dtn-standardisation) | **Missing** (Not Implemented) |
| **Custody Transfer** | [REQ-4](requirements.md#req-4-alignment-with-on-going-dtn-standardisation) | **Missing** (Not Implemented) |
| **PostgreSQL Storage** | [REQ-8](requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage) | **Planned** (Generic Suite Ready) |
| **S3 Storage** | [REQ-9](requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage) | **Planned** (Generic Suite Ready) |
| **Helm Charts** | [REQ-16](requirements.md#req-16-kubernetes-packaging) | **Planned** (Defined in [`PLAN-SERVER-01`](../bpa-server/docs/test_plan.md)) |
| **OCI Packaging** | [REQ-15](requirements.md#req-15-independent-component-packaging) | **Complete** (Defined in [`PLAN-SERVER-01`](../bpa-server/docs/test_plan.md)) |

## 4. Conclusion

The project has a comprehensive verification strategy for all implemented features. The test plans are consistent in format and traceable to the Low-Level Requirements (LLR). The project is **Ready for Test Execution**.
