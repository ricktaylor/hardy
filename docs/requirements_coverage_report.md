# Requirements Coverage Report

> Maps every requirement from [requirements.md](requirements.md) to implementing crates and test evidence.
>
> Initial phase scope: REQ-1, 3, 6, 7, 9, 15, 17, 18, 19, 21. Stretch: REQ-16, 20.
>
> Last updated: 2026-04-14

## Requirements Traceability Matrix

| REQ | Description | Initial phase? | Test Evidence | Status |
| :--- | :--- | :--- | :--- | :--- |
| **1** | **Full compliance with RFC 9171** | Yes | [bpv7 coverage](../bpv7/docs/test_coverage_report.md), [bpa coverage](../bpa/docs/test_coverage_report.md), [cbor coverage](../cbor/docs/test_coverage_report.md) | **Done** |
| 1.1 | Compliance verification matrix | Yes | [PICS Proforma](PICS_Proforma.md) (self-declaration), [PICS Test Mapping](PICS_Test_Mapping.md) | Done |
| 1.2 | Test verification report | Yes | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) §1, [bpa coverage](../bpa/docs/test_coverage_report.md) §1, [cbor coverage](../cbor/docs/test_coverage_report.md) §1 | Done |
| **2** | **Full compliance with RFC 9172/9173** | No | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) | **Done** |
| 2.1 | RFC 9172 compliance matrix | No | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) | Done |
| 2.2 | RFC 9173 compliance matrix | No | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) | Done |
| 2.3 | RFC 9172 test report | No | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) §1 | Done |
| 2.4 | RFC 9173 test report | No | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) §1 | Done |
| **3** | **Full compliance with RFC 9174** | Yes | [tcpclv4 coverage](../tcpclv4/docs/test_coverage_report.md), [tcpclv4-server coverage](../tcpclv4-server/docs/test_coverage_report.md) | **Done** |
| 3.1 | Compliance verification matrix | Yes | [tcpclv4 coverage](../tcpclv4/docs/test_coverage_report.md) §1 | Done |
| 3.2 | Test verification report | Yes | [tcpclv4 coverage](../tcpclv4/docs/test_coverage_report.md) §1, [interop coverage](../tests/interop/docs/test_coverage_report.md) | Done |
| **4** | **Alignment with on-going DTN standardisation** | No | — | **Partial** |
| 4.1 | UDP-CL profile | No | — | Not started (not initial phase) |
| 4.2 | Custody Transfer profile | No | — | Not started (not initial phase) |
| 4.3 | QoS profile | No | EgressPolicy framework in bpa (`policy/`) | In progress (not initial phase) |
| 4.4 | Compressed Status Reporting profile | No | — | Not started (not initial phase) |
| 4.5 | BIBE profile | No | `bibe/` crate ([design](../bibe/docs/design.md)) | In progress (not initial phase) |
| 4.6–4.10 | Test reports for profiles | No | — | Not started (not initial phase) |
| **5** | **Experimental support for QUIC** | No | — | **Not started (not initial phase)** |
| 5.1–5.5 | QUIC specification, support, compliance, testing, config | No | — | Not started (not initial phase) |
| **6** | **Time-variant Routing API** | Yes | [bpa coverage](../bpa/docs/test_coverage_report.md), [tvr coverage](../tvr/docs/test_coverage_report.md) | **Done** |
| 6.1 | Specify contact start | Yes | [tvr coverage](../tvr/docs/test_coverage_report.md) | Done |
| 6.2 | Specify contact duration | Yes | [tvr coverage](../tvr/docs/test_coverage_report.md) | Done |
| 6.3 | Specify expected bandwidth | Yes | [tvr coverage](../tvr/docs/test_coverage_report.md) | Done |
| 6.4 | Specify contact periodicity | Yes | [tvr coverage](../tvr/docs/test_coverage_report.md) | Done |
| 6.5 | Store bundles for expected contact | Yes | [bpa coverage](../bpa/docs/test_coverage_report.md) | Done |
| 6.6 | Update without restart | Yes | [tvr coverage](../tvr/docs/test_coverage_report.md), [bpa-server coverage](../bpa-server/docs/test_coverage_report.md) | Done |
| **7** | **Local filesystem storage** | Yes | [localdisk coverage](../localdisk-storage/docs/test_coverage_report.md), [sqlite coverage](../sqlite-storage/docs/test_coverage_report.md), [storage harness](../tests/storage/docs/test_plan.md) | **Done** |
| 7.1 | Local filesystem bundle storage | Yes | [localdisk coverage](../localdisk-storage/docs/test_coverage_report.md) | Done |
| 7.2 | Local filesystem metadata storage | Yes | [sqlite coverage](../sqlite-storage/docs/test_coverage_report.md) | Done |
| 7.3 | Recovery from local storage | Yes | [storage harness](../tests/storage/docs/test_plan.md) META-05, BLOB-04 | Done |
| **8** | **PostgreSQL metadata storage** | No | [postgres coverage](../postgres-storage/docs/test_coverage_report.md), [storage harness](../tests/storage/docs/test_plan.md) | **Done** |
| 8.1 | PostgreSQL metadata storage | No | [postgres coverage](../postgres-storage/docs/test_coverage_report.md) | Done |
| 8.2 | Recovery from PostgreSQL | No | [storage harness](../tests/storage/docs/test_plan.md) META-05 | Done |
| **9** | **Amazon S3 bundle storage** | Yes | [s3 coverage](../s3-storage/docs/test_coverage_report.md), [storage harness](../tests/storage/docs/test_plan.md) | **Done** |
| 9.1 | S3 bundle storage | Yes | [s3 coverage](../s3-storage/docs/test_coverage_report.md) | Done |
| 9.2 | Recovery from S3 | Yes | [storage harness](../tests/storage/docs/test_plan.md) BLOB-04 | Done |
| **10** | **DynamoDB metadata storage** | No | — | **Not started (not initial phase)** |
| 10.1–10.2 | DynamoDB storage + recovery | No | — | Not started (not initial phase) |
| **11** | **Azure Blob Storage** | No | — | **Not started (not initial phase)** |
| 11.1–11.2 | Azure Blob storage + recovery | No | — | Not started (not initial phase) |
| **12** | **Azure SQL metadata storage** | No | — | **Not started (not initial phase)** |
| 12.1–12.2 | Azure SQL storage + recovery | No | — | Not started (not initial phase) |
| **13** | **Performance** | No | [bpa coverage](../bpa/docs/test_coverage_report.md) §3.3 | **Partial** |
| 13.1 | 1000 bundles/sec | No | [bpa coverage](../bpa/docs/test_coverage_report.md) — criterion benchmark: ~8K/sec | Done |
| 13.2 | 4GB reassembly | No | — | Not tested (not initial phase) |
| 13.3 | 1TB storage for 1 month | No | — | Not tested (not initial phase) |
| 13.4 | 10Gbit/s TCPCLv4 (10MB bundles) | No | — | Not tested (not initial phase) |
| 13.5 | 10Gbit/s TCPCLv4 (1KB bundles) | No | — | Not tested (not initial phase) |
| **14** | **Reliability** | No | Fuzz plans: [bpv7](../bpv7/docs/fuzz_test_plan.md), [cbor](../cbor/docs/fuzz_test_plan.md), [eid-patterns](../eid-patterns/docs/fuzz_test_plan.md), [bpa](../bpa/docs/fuzz_test_plan.md), [tcpclv4](../tcpclv4/docs/fuzz_test_plan.md) | **Partial** |
| 14.1 | Fuzz test verification matrix | No | 5 crates with fuzz targets, 8 fuzz binaries total | Done |
| **15** | **Independent component packaging** | Yes | CI workflows (`docker.yml`, `tools.yml`), [`tests/image_checks.sh`](../tests/image_checks.sh) | **Done** |
| 15.1 | OCI container images | Yes | ghcr.io published (3 images) | Done |
| 15.2 | Install/update/remove verification | Yes | [`tests/image_checks.sh`](../tests/image_checks.sh) | Done |
| 15.3 | Installation documentation | Yes | [Quick Start](https://ricktaylor.github.io/hardy/getting-started/quick-start/), [Docker](https://ricktaylor.github.io/hardy/getting-started/docker/) | Done |
| **16** | **Kubernetes packaging** | Stretch | — | **Not started (stretch)** |
| 16.1 | Helm chart | Stretch | — | Not started (stretch) |
| 16.2 | Installation documentation | Stretch | — | Not started (stretch) |
| **17** | **Comprehensive usage documentation** | Yes | [User docs](https://ricktaylor.github.io/hardy/) | **Done** |
| 17.1 | Overview documentation | Yes | [User Guide](https://ricktaylor.github.io/hardy/) | Done |
| 17.2 | Quick-start guide | Yes | [Quick Start](https://ricktaylor.github.io/hardy/getting-started/quick-start/) | Done |
| 17.3 | Configuration reference | Yes | [Configuration](https://ricktaylor.github.io/hardy/configuration/bpa-server/) | Done |
| **18** | **Technical documentation and examples** | Yes | Design docs across all crates | **Partial** |
| 18.1 | High-level design documentation | Yes | Per-crate `docs/design.md` | Done |
| 18.2 | API documentation + sample code | Yes | — | Not started |
| 18.3 | Failure-mode report | Yes | Planned for user docs (per-backend sections) | Not started |
| 18.4 | Reliability report (MTBF) | No | — | Not started (not initial phase) |
| **19** | **Management and monitoring tools** | Yes | [otel coverage](../otel/docs/test_coverage_report.md), [tools coverage](../tools/docs/test_coverage_report.md) | **Done** |
| 19.1 | OpenTelemetry export | Yes | [otel coverage](../otel/docs/test_coverage_report.md), [`COMP-OTEL-01`](../otel/docs/component_test_plan.md) | Done |
| 19.2 | Network testing tools | Yes | [tools coverage](../tools/docs/test_coverage_report.md) | Done |
| **20** | **Interoperability** | Stretch | [interop coverage](../tests/interop/docs/test_coverage_report.md) | **Done** |
| 20.1 | Interoperability verification matrix | Stretch | [interop coverage](../tests/interop/docs/test_coverage_report.md) §2 | Done |
| 20.2 | Compliance verification matrix | Stretch | [interop coverage](../tests/interop/docs/test_coverage_report.md) §2 | Done |
| **21** | **Permissive licence** | Yes | GitHub, Apache 2.0 | **Done** |
| 21.1 | ESA-compatible licence | Yes | Apache 2.0 | Done |
| 21.2 | Public documentation | Yes | GitHub Pages | Done |
| 21.3 | Issue tracker | Yes | GitHub Issues + `SECURITY.md` | Done |

## Summary

| Category | Total | Done | Partial | Not Started |
| :--- | :--- | :--- | :--- | :--- |
| Initial phase scope (REQ-1,3,6,7,9,15,17,18,19,21) | 10 | 8 | 1 (REQ-18) | 0 |
| Stretch (REQ-16, REQ-20) | 2 | 1 (REQ-20) | 0 | 1 (REQ-16) |
| Full Activity (REQ-2,4,5,8,10,11,12,13,14) | 9 | 3 (REQ-2, REQ-8, REQ-14) | 2 (REQ-4, REQ-13) | 4 |

### Initial Phase Gaps

| REQ | Gap | Severity |
| :--- | :--- | :--- |
| REQ-18.2 | gRPC API usage guide + sample code | Medium |
| REQ-18.3 | Storage failure-mode report | Medium |

## Low-Level Requirements Traceability

This section maps every Low-Level Requirement (LLR) from Part 3 of [requirements.md](requirements.md) to the implementing crate and test evidence. Status is drawn from each crate's test coverage report (linked below).

### Standards Compliance (1.1)

Implementing crate: `bpa` — [coverage report](../bpa/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.1** | CCSDS Bundle Protocol compliance (excluding ADU Fragmentation per RFC 9171 §5.8) | Done |

### CBOR Encoding (1.1)

Implementing crate: `cbor` — [coverage report](../cbor/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.2** | Tagged and untagged data type emission for deterministic CBOR (RFC 8949 §4.2) | Done |
| **1.1.3** | All major types (RFC 8949 §3.1) | Done |
| **1.1.4** | Canonical form emission for deterministic CBOR (RFC 8949 §4.2) | Done |
| **1.1.5** | Correct item count for definite length Maps and Arrays | Done |

### CBOR Decoding (1.1)

Implementing crate: `cbor` — [coverage report](../cbor/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.7** | Report canonical form of parsed data items | Done |
| **1.1.8** | Report associated tags on parsed data items | Done |
| **1.1.9** | All primitive data items (CBOR specification) | Done |
| **1.1.10** | Map/Array context parsing for contained data items | Done |
| **1.1.11** | Opportunistic parsing from byte sequences | Done |
| **1.1.12** | Incomplete data item detection at end of byte sequence | Done |

### CBOR General (1.1)

Implementing crate: `cbor` — [coverage report](../cbor/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.13** | Suitability for embedded platforms (`no_std`) | Done |

### BPv7 Parsing (1.1)

Implementing crate: `bpv7` — [coverage report](../bpv7/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.14** | Bundle rewriting indication during parsing | Done |
| **1.1.15** | Primary block validity indication | Done |
| **1.1.16** | Recognised extension block validity indication | Done |
| **1.1.17** | Bundle validity indication | Done |
| **1.1.18** | Tolerance of unrecognised but correctly encoded flags/type identifiers | Done |
| **1.1.19** | Parse and validate RFC 9171 extension blocks | Done |
| **1.1.20** | Parse and validate extension blocks when specification is available | Done |
| **1.1.21** | Parse and validate all CRC values | Done |
| **1.1.22** | Support all RFC 9171 CRC types | Done |
| **1.1.23** | 3-element CBOR encoding of `ipn` scheme EIDs (RFC 9758) | Done |
| **1.1.24** | Indicate `ipn` EID encoding format (3-element vs legacy 2-element) | Done |

### BPv7 Bundle Generation (1.1)

Implementing crate: `bpv7` — [coverage report](../bpv7/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.25** | Generate valid, canonical CBOR encoded bundles | Done |
| **1.1.26** | Only include valid, canonical CBOR encoded extension blocks | Done |
| **1.1.27** | Apply required CRC values to generated bundles | Done |
| **1.1.28** | Apply required CRC values to generated extension blocks | Done |
| **1.1.29** | Allow caller to specify CRC type for new bundles | Done |

### BPv7 Bundle Processing (1.1)

Implementing crate: `bpa` — [coverage report](../bpa/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.30** | Enforce rewriting rules when discarding unrecognised extension blocks | Done (via bpv7) |
| **1.1.31** | Rewrite non-canonical bundles into canonical form when allowed by policy | Done (via bpv7) |
| **1.1.32** | Indicate reason for rewriting for security policy enforcement | Done (via bpv7) |
| **1.1.33** | Recognise and process Bundle Age extension block for lifetime expiry | Done |
| **1.1.34** | Process and act on Hop Count extension block | Done (via bpv7) |

### BPSec (2.1)

Implementing crate: `bpv7` — [coverage report](../bpv7/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **2.1.1** | Validate BPSec integrity and confidentiality blocks (RFC 9172) | Done |
| **2.1.2** | Remove BPSec target information when targeted block is removed | Done |
| **2.1.3** | Validate fragment + BPSec restriction | N/A |

Note: LLR 2.1.3 is a sender constraint enforced by `signer.rs:75`, not a parser validation. LLR to be corrected.

### RFC 9173 Security Contexts (2.2)

Implementing crate: `bpv7` — [coverage report](../bpv7/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **2.2.1** | BIB-HMAC-SHA2 context with 256-bit hash | Done |
| **2.2.2** | BIB-HMAC-SHA2 context with 384-bit hash | Done |
| **2.2.3** | BIB-HMAC-SHA2 context with 512-bit hash | Done |
| **2.2.4** | Key-wrap function on HMAC key | Done |
| **2.2.5** | BCB-AES-GCM context with 128-bit symmetric key | Done |
| **2.2.6** | BCB-AES-GCM context with 256-bit symmetric key | Done |
| **2.2.7** | Key-wrap function on AES key | Done |

### TCPCLv4 (3.1)

Implementing crate: `tcpclv4` — [coverage report](../tcpclv4/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **3.1.1** | Active session establishment (RFC 9174 §3) | Done |
| **3.1.2** | Passive session establishment (RFC 9174 §3) | Done |
| **3.1.3** | Pool of idle connections for reuse | Done |
| **3.1.4** | Local node IDs in session initialization (RFC 9174 §4.6) | Done |
| **3.1.5** | Configurable session parameter defaults (RFC 9174 §4.7) | Done |
| **3.1.6** | Extension items in session initialization (RFC 9174 §4.8) | Done |
| **3.1.7** | TLS support (RFC 9174 §4.4) | Done |
| **3.1.8** | TLS enabled by default | Done |
| **3.1.9** | TLS Entity Identification via DNS Name and Network Address (RFC 9174 §4.4.1) | Done |
| **3.1.10** | Session upkeep messages when negotiated | Done |

### EID Patterns (6.1)

Implementing crate: `eid-patterns` — [coverage report](../eid-patterns/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **6.1.1** | Parse textual representation of `ipn` EID patterns | Done |
| **6.1.2** | Determine if a particular EID and EID pattern match | Done |

### CLA APIs (6.1)

Implementing crate: `bpa` — [coverage report](../bpa/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **6.1.3** | API for CLAs to indicate forwarding success | Done |
| **6.1.4** | API for EID-to-CLA address resolution (e.g. DNS lookup) | Done |

### Routing (6.1)

Implementing crate: `bpa` — [coverage report](../bpa/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **6.1.5** | Routing information via configuration file | Done |
| **6.1.6** | API for runtime addition and removal of routes | Done |
| **6.1.7** | Discard bundles based on destination | Done |
| **6.1.8** | Reflect bundle back to previous node on per-bundle basis | Done |
| **6.1.9** | Prioritise routing rules to avoid misconfiguration | Done |
| **6.1.10** | Equal Cost Multi-Path (ECMP) for same-priority CLAs | Done |

### Local Disk Storage (7.1)

Implementing crate: `localdisk-storage` — [coverage report](../localdisk-storage/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **7.1.1** | Configurable storage location | Done |
| **7.1.2** | Configurable maximum total for stored bundle data | Not tested |
| **7.1.3** | Configurable discard mechanism at capacity | Done (via harness) |

Note: LLR 7.1.2 enforcement is in the BPA layer, not the storage backend.

### SQLite Storage (7.2)

Implementing crate: `sqlite-storage` — [coverage report](../sqlite-storage/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **7.2.1** | Store and retrieve metadata from SQLite database | Done (via harness) |
| **7.2.2** | Configurable filesystem location for metadata database | Done |

### S3 Storage (9.1)

Implementing crate: `s3-storage` — [coverage report](../s3-storage/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **9.1.1** | Configurable location and access credentials for S3 instance | Not tested |
| **9.1.2** | Configurable maximum total for stored bundle data on S3 | Not tested |
| **9.1.3** | Configurable discard mechanism at S3 capacity | Not tested |
| **9.1.4** | Use common S3 APIs (not implementor-specific) | Done |

Note: LLR 9.1.1 is planned (S3-01) but not yet implemented. LLR 9.1.2/9.1.3 enforcement is in the BPA layer. LLR 9.1.4 is verified by design (uses `aws-sdk-s3`).

### OpenTelemetry (19.1)

Implementing crate: `otel` — [coverage report](../otel/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **19.1.1** | Emit OpenTelemetry Log Records | Done |
| **19.1.2** | Emit OpenTelemetry Traces | Done |
| **19.1.3** | Emit OpenTelemetry Metrics | Done |

### Tools (19.2)

Implementing crate: `tools` — [coverage report](../tools/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **19.2.1** | Rate-controlled bundle sending tool | Done |
| **19.2.2** | Bundle reception response reporting tool | Not tested |
| **19.2.3** | Round-trip communication time reporting tool | Done |
| **19.2.4** | Tools do not rely on BPv7 status reports | Done |
| **19.2.5** | Tools run without requiring a local BPA | Done |

Note: LLR 19.2.2 (`bp perf` receive-side reporting) is planned but not yet implemented with dedicated test coverage.

### Documentation (21.1, 21.2)

Verified by: visual inspection (GitHub, crates.io, docs.rs)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **21.2.1** | Source, documentation, and examples on GitHub.com | Done |
| **21.2.2** | Rust crates available on crates.io | Done |
| **21.2.3** | RustDoc documentation on docs.rs | Done |

### Issue Reporting and Tracking (21.3)

Verified by: visual inspection (GitHub)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **21.3.1** | Public Issue Tracker on GitHub.com | Done |
| **21.3.2** | Private vulnerability reporting via GitHub | Done |

### LLR Traceability Summary

| Section | Mid-Level | Crate | Total LLRs | Done | N/A | Not Tested |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| Standards Compliance | 1.1 | bpa | 1 | 1 | 0 | 0 |
| CBOR Encoding | 1.1 | cbor | 4 | 4 | 0 | 0 |
| CBOR Decoding | 1.1 | cbor | 6 | 6 | 0 | 0 |
| CBOR General | 1.1 | cbor | 1 | 1 | 0 | 0 |
| BPv7 Parsing | 1.1 | bpv7 | 11 | 11 | 0 | 0 |
| BPv7 Bundle Generation | 1.1 | bpv7 | 5 | 5 | 0 | 0 |
| BPv7 Bundle Processing | 1.1 | bpa | 5 | 5 | 0 | 0 |
| BPSec | 2.1 | bpv7 | 3 | 2 | 1 | 0 |
| RFC 9173 Security Contexts | 2.2 | bpv7 | 7 | 7 | 0 | 0 |
| TCPCLv4 | 3.1 | tcpclv4 | 10 | 10 | 0 | 0 |
| EID Patterns | 6.1 | eid-patterns | 2 | 2 | 0 | 0 |
| CLA APIs | 6.1 | bpa | 2 | 2 | 0 | 0 |
| Routing | 6.1 | bpa | 6 | 6 | 0 | 0 |
| Local Disk Storage | 7.1 | localdisk-storage | 3 | 2 | 0 | 1 |
| SQLite Storage | 7.2 | sqlite-storage | 2 | 2 | 0 | 0 |
| S3 Storage | 9.1 | s3-storage | 4 | 1 | 0 | 3 |
| OpenTelemetry | 19.1 | otel | 3 | 3 | 0 | 0 |
| Tools | 19.2 | tools | 5 | 4 | 0 | 1 |
| Documentation | 21.1, 21.2 | (GitHub/crates.io) | 3 | 3 | 0 | 0 |
| Issue Reporting | 21.3 | (GitHub) | 2 | 2 | 0 | 0 |
| **Total** | | | **78** | **72** | **1** | **5** |
