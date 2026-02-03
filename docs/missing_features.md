# Missing Features Report

| Document Info | Details |
| :--- | :--- |
| **Project** | Hardy |
| **Source** | Analysis of [requirements.md](requirements.md) vs [PICS_Proforma.md](PICS_Proforma.md) and Source Code |
| **Date** | 2026-01-19 |

## 1. High-Level Requirements Gaps

The following features are listed in the Requirements Traceability Matrix (RTM) but do not appear to be implemented in the current codebase.

| ID | Category | Priority | Feature | Status | Evidence |
| :--- | :--- | :--- | :--- | :--- | :--- |
| [**REQ-4**](requirements.md#req-4-alignment-with-on-going-dtn-standardisation) | Standards | Desirable | **UDP Convergence Layer** | Not Implemented | PICS Item 25 is 'N'. |
| [**REQ-4**](requirements.md#req-4-alignment-with-on-going-dtn-standardisation) | Standards | Desirable | **Custody Transfer** | Not Implemented | PICS Item 28 (Managed Info) is 'N'. |
| [**REQ-5**](requirements.md#req-5-experimental-support-for-quic) | Transport | Optional | **QUIC Convergence Layer** | Not Implemented | No `hardy-quic` crate exists. |
| [**REQ-8**](requirements.md#req-8-support-for-postgresql-for-bundle-metadata-storage) | Storage | Desirable | **PostgreSQL Metadata** | Planned | Listed as "Planned" in `storage_integration_test_plan.md`. |
| [**REQ-9**](requirements.md#req-9-support-for-amazon-s3-storage-for-bundle-storage) | Storage | Desirable | **Amazon S3 Storage** | Planned | Listed as "Planned" in `storage_integration_test_plan.md`. |
| [**REQ-10**](requirements.md#req-10-support-for-amazon-dynamodb-for-bundle-metadata-storage) | Storage | Desirable | **DynamoDB Metadata** | Not Implemented | No references in code or plans. |
| [**REQ-11**](requirements.md#req-11-support-for-azure-blob-storage-for-bundle-storage) | Storage | Desirable | **Azure Blob Storage** | Not Implemented | No references in code or plans. |
| [**REQ-12**](requirements.md#req-12-support-for-azure-sql-for-bundle-metadata-storage) | Storage | Desirable | **Azure SQL Metadata** | Not Implemented | No references in code or plans. |
| [**REQ-16**](requirements.md#req-16-kubernetes-packaging) | Deployment | Desirable | **Helm Charts** | Not Implemented | No `charts/` directory or packaging scripts found. |

## 2. Protocol Compliance Gaps (RFC 9171)

While **REQ-1** mandates full compliance, the following optional BPv7 features are explicitly marked as unsupported in the PICS.

| PICS Item | Feature | Description | Impact |
| :--- | :--- | :--- | :--- |
| **24** | **LTP CLA** | Licklider Transmission Protocol. | Cannot communicate over deep-space links requiring LTP. |
| **26** | **Space Packets** | CCSDS Space Packet Encapsulation. | Cannot interface directly with legacy spacecraft busses. |
| **28** | **Managed Info** | BP Managed Information (Annex C). | Required for Custody Transfer reporting. |
| **40** | **Fragmentation** | Generation of Bundle Fragments. | Router cannot proactively fragment large bundles to fit MTUs (Reassembly is supported). |
