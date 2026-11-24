# Requirements Coverage Report

| Document Info | Details |
| :--- | :--- |
| **Project** | Hardy DTN Framework |
| **Requirements** | [requirements.md](requirements.md) version 1.3 |

## 1. Introduction

This report maps every requirement from [requirements.md](requirements.md) to implementing crates and test evidence. It provides end-to-end traceability from top-level requirements (Part 2) through mid-level verification requirements (Part 4) to Low-Level Requirements (Part 3) and individual test results.

**Overall status:**

* 23 top-level requirements, 3 of them withdrawn (REQ-10, 11, 12)
  * 10 Done
  * 9 Partial
  * 1 Not started
* 168 LLRs
  * 124 Done
  * 3 Implemented, not yet traced to a test
  * 6 Withdrawn
  * 35 Not implemented
* Gaps and implementation status detailed in [§4](#4-gaps)

## 2. Requirements Traceability Matrix

| REQ | Description | Test Evidence | Status |
| :--- | :--- | :--- | :--- |
| **1** | **Full compliance with RFC 9171** | [bpv7 coverage](../bpv7/docs/test_coverage_report.md), [bpa coverage](../bpa/docs/test_coverage_report.md), [cbor coverage](../cbor/docs/test_coverage_report.md) | **Done** |
| 1.1 | Compliance verification matrix | [PICS Proforma](PICS_Proforma.md) (self-declaration), [PICS Test Mapping](PICS_Test_Mapping.md) | Done |
| 1.2 | Test verification report | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) §1, [bpa coverage](../bpa/docs/test_coverage_report.md) §1, [cbor coverage](../cbor/docs/test_coverage_report.md) §1 | Done |
| **2** | **Full compliance with RFC 9172/9173** | [bpv7 coverage](../bpv7/docs/test_coverage_report.md), [BPSec PICS](../bpv7/docs/bpsec-pics.md) | **Partial** |
| 2.1 | RFC 9172 compliance matrix | [BPSec PICS](../bpv7/docs/bpsec-pics.md) | Done |
| 2.2 | RFC 9173 compliance matrix | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) | Done |
| 2.3 | RFC 9172 test report | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) §1 | Done |
| 2.4 | RFC 9173 test report | [bpv7 coverage](../bpv7/docs/test_coverage_report.md) §1 | Done |
| **3** | **Full compliance with RFC 9174** | [tcpclv4 coverage](../tcpclv4/docs/test_coverage_report.md), [tcpclv4-server coverage](../tcpclv4-server/docs/test_coverage_report.md) | **Done** |
| 3.1 | Compliance verification matrix | [tcpclv4 coverage](../tcpclv4/docs/test_coverage_report.md) §1 | Done |
| 3.2 | Test verification report | [tcpclv4 coverage](../tcpclv4/docs/test_coverage_report.md) §1, [interop coverage](../tests/interop/docs/test_coverage_report.md) | Done |
| **4** | **Alignment with on-going DTN standardisation** | `bibe/` crate | **Partial** |
| 4.1 | UDP-CL profile | — | Not started |
| 4.2 | Custody Transfer profile | Maintained outside the workspace ([extensions registry](extensions.md)) | Not started |
| 4.3 | QoS profile | `FlowControllerFactory` framework in bpa (`policy/`) | In progress |
| 4.4 | Compressed Status Reporting profile | — | Not started |
| 4.5 | BIBE profile | `bibe/` crate ([design](../bibe/docs/design.md)) | In progress |
| 4.6–4.10 | Test reports for profiles | — | Not started |
| 4.11 | BPSec COSE profile | — | Not started |
| 4.12 | BP Managed Information profile | — | Not started |
| **5** | **Experimental and pre-standard convergence layer protocols** | [btpu coverage](../btpu/docs/test_coverage_report.md) | **Partial** |
| 5.1–5.5 | QUIC specification, support, compliance, testing, config | — | Not started |
| 5.6 | BTP-U support | Protocol engine done (`btpu` crate); no convergence layer adaptor yet (LLR 5.6.3) | Partial |
| 5.7 | BTP-U compliance matrix | [btpu coverage](../btpu/docs/test_coverage_report.md) §1 (traced to draft sections) | Done |
| 5.8 | BTP-U test report | [btpu coverage](../btpu/docs/test_coverage_report.md) | Done |
| 5.9 | BTP-U runtime configuration | — (needs the convergence layer adaptor) | Not started |
| **6** | **Time-variant Routing API** | [bpa coverage](../bpa/docs/test_coverage_report.md), [tvr coverage](../tvr/docs/test_coverage_report.md) | **Partial** |
| 6.1 | Specify contact start | [tvr coverage](../tvr/docs/test_coverage_report.md) | Done |
| 6.2 | Specify contact duration | [tvr coverage](../tvr/docs/test_coverage_report.md) | Done |
| 6.3 | Specify expected bandwidth | [tvr coverage](../tvr/docs/test_coverage_report.md) | Done |
| 6.4 | Specify contact periodicity | [tvr coverage](../tvr/docs/test_coverage_report.md) | Done |
| 6.5 | Store bundles for expected contact | [bpa coverage](../bpa/docs/test_coverage_report.md) | Done |
| 6.6 | Update without restart | [tvr coverage](../tvr/docs/test_coverage_report.md), [bpa-server coverage](../bpa-server/docs/test_coverage_report.md) | Done |
| 6.7 | Apply contact bandwidth to forwarding | — (LLR 13.1.6, not implemented) | Not started |
| **7** | **Local filesystem storage** | [localdisk coverage](../localdisk-storage/docs/test_coverage_report.md), [sqlite coverage](../sqlite-storage/docs/test_coverage_report.md), [storage harness](../tests/storage/docs/test_plan.md) | **Done** |
| 7.1 | Local filesystem bundle storage | [localdisk coverage](../localdisk-storage/docs/test_coverage_report.md) | Done |
| 7.2 | Local filesystem metadata storage | [sqlite coverage](../sqlite-storage/docs/test_coverage_report.md) | Done |
| 7.3 | Recovery from local storage | [storage harness](../tests/storage/docs/test_plan.md) META-05, BLOB-04 | Done |
| **8** | **PostgreSQL metadata storage** | [postgres coverage](../postgres-storage/docs/test_coverage_report.md), [storage harness](../tests/storage/docs/test_plan.md) | **Done** |
| 8.1 | PostgreSQL metadata storage | [postgres coverage](../postgres-storage/docs/test_coverage_report.md) | Done |
| 8.2 | Recovery from PostgreSQL | [storage harness](../tests/storage/docs/test_plan.md) META-05 | Done |
| **9** | **Amazon S3 bundle storage** | [s3 coverage](../s3-storage/docs/test_coverage_report.md), [storage harness](../tests/storage/docs/test_plan.md) | **Done** |
| 9.1 | S3 bundle storage | [s3 coverage](../s3-storage/docs/test_coverage_report.md) | Done |
| 9.2 | Recovery from S3 | [storage harness](../tests/storage/docs/test_plan.md) BLOB-04 | Done |
| **10** | **DynamoDB metadata storage** | — | **Withdrawn** |
| **11** | **Azure Blob Storage** | — | **Withdrawn** |
| **12** | **Azure SQL metadata storage** | — | **Withdrawn** |
| **13** | **Performance** | [bpa coverage](../bpa/docs/test_coverage_report.md) §3.3 | **Partial** |
| 13.1 | 1000 bundles/sec | [bpa coverage](../bpa/docs/test_coverage_report.md): criterion benchmark, about 8K/sec | Done |
| 13.2 | 4GB reassembly | — | Not tested |
| 13.3 | 1TB storage for 1 month | — | Not tested |
| 13.4 | 10Gbit/s TCPCLv4 (10MB bundles) | — | Not tested |
| 13.5 | 10Gbit/s TCPCLv4 (1KB bundles) | — | Not tested |
| 13.6 | Single 4GB streamed bundle | — (LLR 1.2.8, not implemented) | Not started |
| **14** | **Reliability** | Fuzz plans: [bpv7](../bpv7/docs/fuzz_test_plan.md), [cbor](../cbor/docs/fuzz_test_plan.md), [eid-patterns](../eid-patterns/docs/fuzz_test_plan.md), [bpa](../bpa/docs/fuzz_test_plan.md), [tcpclv4](../tcpclv4/docs/fuzz_test_plan.md), [btpu](../btpu/docs/fuzz_test_plan.md) | **Partial** |
| 14.1 | Fuzz test verification matrix | 6 crates with fuzz targets, 12 fuzz binaries total | Done |
| 14.2 | Soak test report | Scenarios defined in [bpa-server test plan](../bpa-server/docs/test_plan.md) §4.6, not yet run | Not started |
| **15** | **Independent component packaging** | CI workflow (`oci-images.yml`), [`tests/image_checks.sh`](../tests/image_checks.sh) | **Done** |
| 15.1 | OCI container images | ghcr.io published (4 images: bpa-server, tcpclv4-server, tvr, tools) | Done |
| 15.2 | Install/update/remove verification | [`tests/image_checks.sh`](../tests/image_checks.sh) | Done |
| 15.3 | Installation documentation | [Quick Start](https://ricktaylor.github.io/hardy/getting-started/quick-start/), [Docker](https://ricktaylor.github.io/hardy/getting-started/docker/) | Done |
| **16** | **Kubernetes packaging** | — | **Not started** |
| 16.1 | Helm chart | — | Not started |
| 16.2 | Installation documentation | — | Not started |
| **17** | **Comprehensive usage documentation** | [User docs](https://ricktaylor.github.io/hardy/) | **Done** |
| 17.1 | Overview documentation | [User Guide](https://ricktaylor.github.io/hardy/) | Done |
| 17.2 | Quick-start guide | [Quick Start](https://ricktaylor.github.io/hardy/getting-started/quick-start/) | Done |
| 17.3 | Configuration reference | [Configuration](https://ricktaylor.github.io/hardy/configuration/bpa-server/) | Done |
| **18** | **Technical documentation and examples** | Design docs across all crates | **Done** |
| 18.1 | High-level design documentation | Per-crate `docs/design.md` | Done |
| 18.2 | API documentation + sample code | gRPC APIs: [proto README](../proto/README.md), [tvr README](../tvr/README.md), generated [API reference](../proto/docs/api_reference.md). Rust APIs: RustDoc for every library crate on [docs.rs](https://docs.rs/hardy-bpa), including the extension-point traits (see 21.2.3, 23.1.4) | Done |
| 18.3 | Failure-mode report | [Recovery guide](https://ricktaylor.github.io/hardy/recovery/) | Done |
| 18.4 | Reliability report (MTBF) | — | Withdrawn (replaced by 14.2 and 18.3) |
| **19** | **Management and monitoring tools** | [otel coverage](../otel/docs/test_coverage_report.md), [tools coverage](../tools/docs/test_coverage_report.md) | **Done** |
| 19.1 | OpenTelemetry export | [otel coverage](../otel/docs/test_coverage_report.md), [`COMP-OTEL-01`](../otel/docs/component_test_plan.md). Metrics: BPA (33), TCPCLv4 (11), TVR (7) | Done |
| 19.2 | Network testing tools | [tools coverage](../tools/docs/test_coverage_report.md) | Done |
| **20** | **Interoperability** | [interop coverage](../tests/interop/docs/test_coverage_report.md), [interop results](../tests/interop/interop_results.md) | **Partial** |
| 20.1 | Interoperability verification matrix | [interop coverage](../tests/interop/docs/test_coverage_report.md) §2; 7 of 8 implementations (no Unibo-BP harness yet) | Partial |
| 20.2 | Compliance verification matrix | [interop coverage](../tests/interop/docs/test_coverage_report.md) §2; 7 of 8 implementations | Partial |
| **21** | **Permissive licence** | GitHub, Apache 2.0 | **Done** |
| 21.1 | ESA-compatible licence | Apache 2.0 | Done |
| 21.2 | Public documentation | GitHub Pages | Done |
| 21.3 | Issue tracker | GitHub Issues + `SECURITY.md` | Done |
| **22** | **Embedded and constrained platform support** | CI `no-std` job (`.github/workflows/rust.yml`) | **Partial** |
| 22.1 | Bare-metal 32-bit build in CI | `thumbv7em-none-eabihf`: cbor, btpu, bpv7, eid-patterns; `thumbv6m-none-eabi`: btpu | Done for 4 of 5 crates (bpa not yet) |
| 22.2 | Embedded verification matrix | This report, §3 Embedded Platforms (22.1) | Done |
| **23** | **Composable framework APIs** | This report, §3 Framework APIs (23.1) | **Partial** |
| 23.1 | Reference applications built from public APIs in CI | Workspace build in `.github/workflows/rust.yml` | Done |
| 23.2 | Extension point verification matrix | This report, §3 Framework APIs (23.1); code examples missing (LLR 23.1.7) | Partial |
| 23.3 | Developer guide | — (LLR 23.1.8, not implemented) | Not started |

## 3. Low-Level Requirements Traceability

This section maps every Low-Level Requirement (LLR) from Part 3 of [requirements.md](requirements.md) to the implementing crate and test evidence. Status is drawn from each crate's test coverage report (linked below) and, for LLRs added in version 1.3, from the named tests.

### Standards Compliance (1.1)

Implementing crate: `bpa`, [coverage report](../bpa/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.1** | CCSDS Bundle Protocol compliance (excluding ADU Fragmentation per RFC 9171 §5.8) | Done |

### CBOR Encoding (1.1)

Implementing crate: `cbor`, [coverage report](../cbor/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.2** | Tagged and untagged data type emission for deterministic CBOR (RFC 8949 §4.2) | Done |
| **1.1.3** | All major types (RFC 8949 §3.1) | Done |
| **1.1.4** | Canonical form emission for deterministic CBOR (RFC 8949 §4.2) | Done |
| **1.1.5** | Correct item count for definite length Maps and Arrays | Done |

### CBOR Decoding (1.1)

Implementing crate: `cbor`, [coverage report](../cbor/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.7** | Report canonical form of parsed data items | Done |
| **1.1.8** | Report associated tags on parsed data items | Done |
| **1.1.9** | All primitive data items (CBOR specification) | Done |
| **1.1.10** | Map/Array context parsing for contained data items | Done |
| **1.1.11** | Opportunistic parsing from byte sequences | Done |
| **1.1.12** | Incomplete data item detection at end of byte sequence | Done |

### CBOR General (1.1)

Implementing crate: `cbor`, [coverage report](../cbor/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.13** | Suitability for embedded platforms (`no_std`) | Done |

### BPv7 Parsing (1.1)

Implementing crate: `bpv7`, [coverage report](../bpv7/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.14** | Bundle rewriting indication during parsing | Withdrawn |
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

Implementing crate: `bpv7`, [coverage report](../bpv7/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.25** | Generate valid, canonical CBOR encoded bundles | Done |
| **1.1.26** | Only include valid, canonical CBOR encoded extension blocks | Done |
| **1.1.27** | Apply required CRC values to generated bundles | Done |
| **1.1.28** | Apply required CRC values to generated extension blocks | Done |
| **1.1.29** | Allow caller to specify CRC type for new bundles | Done |

### BPv7 Bundle Processing (1.1)

Implementing crate: `bpa`, [coverage report](../bpa/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **1.1.30** | Remove unrecognised blocks from forwarded and delivered copies; stored bytes unchanged | Done |
| **1.1.31** | Reject non-canonical input; never rewrite it | Done (via bpv7) |
| **1.1.32** | Report the reason each block is scheduled for removal | Done (via bpv7: `checks.rs::classify_reports_the_facts_with_the_delete_verdict`, `classify_reports_the_security_facts_with_the_delete_verdict`) |
| **1.1.33** | Recognise and process Bundle Age extension block for lifetime expiry | Done |
| **1.1.34** | Process and act on Hop Count extension block | Done (via bpv7) |

### Bundle Pipeline (1.2)

Implementing crate: `bpa`. Tests are in `bpa/tests/pipeline.rs` unless another file is named.

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **1.2.1** | Rejected bundles are never stored | Done | `early_route_drop_never_awaits_the_payload`, `duplicate_arrival_never_awaits_the_payload`, `gate_reports_hop_exhaustion_but_not_expiry` |
| **1.2.2** | Truncated or oversized bundles refused without a report | Done | `cla_streamed_ingress_truncation_is_refused`, `an_abandoned_short_transfer_is_refused_and_unreported`, `ingress_size_cap_refuses_oversized_bundle` |
| **1.2.3** | Combined reception and deletion report on rejection | Done | `header_failure_reports_reception_then_deletion`, `drain_failure_reports_reception_then_deletion`, `step4_block_flag_alone_forces_reception_report` |
| **1.2.4** | Forwarding and delivery reports | Not Tested | Implemented in `src/dispatcher/report.rs`; no test asserts the forwarded or delivered assertion |
| **1.2.5** | Expiry in storage deletes with a report | Done | `src/dispatcher/forward.rs::expired_queued_bundle_is_never_offered`; `tests/on_deliver.rs::deferred_delivery_expires_honestly`, `expiry_mid_delivery_resolves_once` |
| **1.2.6** | Originated bundles validated and filtered like received ones | Done | `raw_originate_rejects_a_fragment`, `originate_route_drop_never_awaits_the_payload`; `tests/filter_dispositions.rs::an_originate_classifier_reads_its_declared_peek` |
| **1.2.7** | ADU reassembly for local delivery | Done | `src/storage/adu_reassembly.rs::reassemble_basic_happy_path` and the `reassemble_rejects_*` cases |
| **1.2.8** | Per-bundle memory bounded independently of payload size | Not implemented | — |
| **1.2.9** | Bounded reassembly, overlapping fragments, payload BIB on fragments | Not implemented | — |
| **1.2.10** | Originate and Deliver drop reports | Not implemented | — |

### Bundle Filters (1.3)

Implementing crate: `bpa`. Tests are in `bpa/tests/filter_dispositions.rs` unless another file is named.

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **1.3.1** | Ingress, Originate, Egress and Deliver hooks for integrator filters | Done | `src/filter/engine.rs` tests; this file |
| **1.3.2** | Verifiers drop with a reason | Done | `ingress_drop_with_a_reason_reports_before_storing`, `deliver_drop_with_a_reason_reports_once` |
| **1.3.3** | Classifiers drop or annotate; annotations persist | Done | `src/filter/engine.rs::classifier_sees_preceding_deltas_and_result_persists`; `tests/pipeline.rs::classifier_route_key_drives_the_lookup` |
| **1.3.4** | Rewriters add, replace and remove extension blocks | Done | `bpv7/tests/editor.rs::extension_editor_materialises_inserts_and_skips_untouched`; `egress_rewriter_meets_a_refusal_on_an_admin_record` |
| **1.3.5** | Filters read blocks and a bounded payload prefix | Done | `an_ingress_classifier_reads_its_declared_peek` |
| **1.3.6** | Re-classification of stored bundles on filter change | Not implemented | — |
| **1.3.7** | External inspection of selected bundles | Not implemented | — |

### BPSec (2.1)

Implementing crates: `bpv7`, `bpa`, `bpa-server`. [bpv7 coverage report](../bpv7/docs/test_coverage_report.md), [BPSec PICS](../bpv7/docs/bpsec-pics.md)

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **2.1.1** | Validate BPSec integrity and confidentiality blocks (RFC 9172) | Done | bpv7 coverage report |
| **2.1.2** | Remove BPSec target information when targeted block is removed | Done | bpv7 coverage report |
| **2.1.3** | No BPSec blocks added to a fragment | Done | `tests/bpsec/tests/pics_default.rs::pics_56_1_cannot_add_bcb_to_fragment`, `pics_56_1b_cannot_add_bib_to_fragment` |
| **2.1.4** | Signing as security source | Done | `bpv7/tests/rfc9173.rs::test_sign_then_encrypt`, `test_sign_primary_block_with_crc`; `pics_2_1_source_sign_payload_and_bundle_age` |
| **2.1.5** | Encryption as security source | Done | `pics_2_3_source_encrypt_payload_and_bundle_age`, `bpv7/tests/rfc9173.rs::test_sign_then_encrypt` |
| **2.1.6** | Integrator-supplied key material | Done | `bpa/tests/forward.rs` and `bpa/tests/pipeline.rs` key-provider fixtures; `bpa-server/src/bpsec.rs` tests |
| **2.1.7** | Keyless forwarding; Bundle Age liveness rule | Done | `bpa/tests/forward.rs::keyless_relay_forwards_a_signed_then_encrypted_bundle`, `keyless_relay_forwards_an_encrypted_hop_count_unchanged`; `bpa/src/bundle/parse.rs::nokey_hop_count_passes_ingress_unread` |
| **2.1.8** | Verify on reception; RFC 9172 §5.1.1 failure handling | Done | `bpa/src/dispatcher/validate.rs::deferred_bib_verifies_over_stream`, `deferred_bib_tamper_fails`; `bpv7/tests/checks.rs::corrupt_covered_bib_is_failure_dropped` |
| **2.1.9** | Decrypted payload never exposed to filters | Done | `bpa/tests/filter_dispositions.rs::an_encrypted_payload_has_no_peek` |
| **2.1.10** | Key material kept out of logs and diagnostics, not retained after use | Not Tested | `SecretBytes` self-redacts and zeroizes (`bpv7/src/bpsec/key.rs`); no test asserts its `Debug` output |
| **2.1.11** | BPA as security source and waypoint acceptor by policy | Not implemented | — |
| **2.1.12** | Delivery once a missing decryption key arrives | Not implemented | — |
| **2.1.13** | Bounded memory for payload encryption and decryption; authenticated before delivery | Not implemented | — |
| **2.1.14** | RFC 9172 security reason codes in status reports | Not implemented | — |
| **2.1.15** | Configurable security failure and protection-required policy | Not implemented | — |
| **2.1.16** | BPSec COSE security contexts | Not implemented | — |

### RFC 9173 Security Contexts (2.2)

Implementing crate: `bpv7`, [coverage report](../bpv7/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **2.2.1** | BIB-HMAC-SHA2 context with 256-bit hash | Done |
| **2.2.2** | BIB-HMAC-SHA2 context with 384-bit hash | Done |
| **2.2.3** | BIB-HMAC-SHA2 context with 512-bit hash | Done |
| **2.2.4** | Key-wrap function on HMAC key | Done |
| **2.2.5** | BCB-AES-GCM context with 128-bit symmetric key | Done |
| **2.2.6** | BCB-AES-GCM context with 256-bit symmetric key | Done |
| **2.2.7** | Key-wrap function on AES key | Done |
| **2.2.8** | RFC 9173 erratum 9192 primary-block IPPT | Not implemented |

### TCPCLv4 (3.1)

Implementing crate: `tcpclv4`, [coverage report](../tcpclv4/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **3.1.1** | Active session establishment (RFC 9174 §3) | Done |
| **3.1.2** | Passive session establishment (RFC 9174 §3) | Done |
| **3.1.3** | Pool of idle connections for reuse | Withdrawn |
| **3.1.4** | Local node IDs in session initialization (RFC 9174 §4.6) | Done |
| **3.1.5** | Configurable session parameter defaults (RFC 9174 §4.7) | Done |
| **3.1.6** | Extension items in session initialization (RFC 9174 §4.8) | Done |
| **3.1.7** | TLS support (RFC 9174 §4.4) | Done |
| **3.1.8** | TLS enabled by default | Done |
| **3.1.9** | TLS Entity Identification via DNS Name and Network Address (RFC 9174 §4.4.1) | Done |
| **3.1.10** | Session upkeep messages when negotiated | Done |
| **3.1.11** | Node ID authentication against the certificate | Not implemented |
| **3.1.12** | Retransmittable refusal of incomplete transfers | Not implemented |

### Bundle-in-Bundle Encapsulation (4.5)

Implementing crate: `bibe`

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **4.5.1** | Bundle-in-Bundle Encapsulation | Done | `bibe/src/pdu.rs::test_encap_decap_round_trip` |
| **4.5.2** | Segmented Bundle-in-Bundle Encapsulation | Not implemented | — |
| **4.5.3** | Inner bundle not lost on transient decapsulation failure | Not implemented | — |

### BTP-U (5.6)

Implementing crate: `btpu`, [coverage report](../btpu/docs/test_coverage_report.md)

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **5.6.1** | BTP-U engine carrying bundles over link frames | Done | btpu coverage report §1 (-04 §4, §8.1–8.3) |
| **5.6.2** | BTP-U receiver withstands malformed, repeated or flooding input | Done | btpu coverage report §1 (-04 §7.3, §10) |
| **5.6.3** | BTP-U convergence layer adaptor | Not implemented | — |

### EID Patterns (6.1)

Implementing crate: `eid-patterns`, [coverage report](../eid-patterns/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **6.1.1** | Parse textual representation of `ipn` EID patterns | Done |
| **6.1.2** | Determine if a particular EID and EID pattern match | Done |
| **6.1.11** | Optional `dtn` scheme EID patterns | Done (`tests/patterns.rs::dtn_glob_match`, `dtn_glob_authority_case_insensitive`) |

### CLA APIs (6.1)

Implementing crate: `bpa`, [coverage report](../bpa/docs/test_coverage_report.md)

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **6.1.3** | API for CLAs to indicate forwarding success | Done | bpa coverage report |
| **6.1.4** | EID resolution to CLA addresses | Not implemented | [Dial-on-demand plan](../bpa/docs/dial_on_demand_plan.md) |
| **6.1.12** | CLA peer reporting | Done | `proto` `cla_cli_01..05` |
| **6.1.13** | Failed transfer returns to routing | Done | `tests/pipeline.rs::deferred_outcome_completed_resolves`, `deferred_outcome_failed_redispatches`, `deferred_outcome_peer_removal_resolves_unknown` |
| **6.1.14** | CLA told of acceptance or refusal | Done | `tests/pipeline.rs::cla_streamed_ingress_truncation_is_refused`, `cla_segmented_without_final_is_refused` |
| **6.1.15** | Adjacencies on demand | Not implemented | — |
| **6.1.16** | Unresolved transfers never strand their bundle | Not implemented | — |

### Routing (6.1)

Implementing crates: `bpa`, `bpa-server`. [bpa coverage report](../bpa/docs/test_coverage_report.md)

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **6.1.5** | Routing information via configuration file | Done | bpa coverage report |
| **6.1.6** | API for runtime addition and removal of routes | Done | bpa coverage report |
| **6.1.7** | Discard bundles based on destination | Done | bpa coverage report |
| **6.1.8** | Reflect bundle back to previous node on per-bundle basis | Done | bpa coverage report |
| **6.1.9** | Prioritise routing rules to avoid misconfiguration | Done | bpa coverage report |
| **6.1.10** | Equal Cost Multi-Path (ECMP) for same-priority CLAs | Done | bpa coverage report |
| **6.1.17** | No route means wait, not drop | Done | `tests/pipeline.rs::forward_failure_park_recheck_redispatches`, `a_keyed_local_destination_waits_for_its_key_route`; storage harness META-06, META-14 |
| **6.1.18** | Routing agent API, in-process and gRPC | Done | `proto` `rte_cli_01..03`; [tvr coverage](../tvr/docs/test_coverage_report.md) |
| **6.1.19** | Static routes reloaded on file change | Done | `bpa-server/tests/test_static_routes.sh` TEST 2, TEST 4 |
| **6.1.20** | Malformed routes file keeps existing routes | Not implemented | — |
| **6.1.21** | Multiple routing tables | Not implemented | — |

### Local Disk Storage (7.1)

Implementing crate: `localdisk-storage`, [coverage report](../localdisk-storage/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **7.1.1** | Configurable storage location | Done |
| **7.1.2** | Configurable maximum total for stored bundle data | Withdrawn (see 7.3.7) |
| **7.1.3** | Configurable discard mechanism at capacity | Withdrawn (see 7.3.7) |

### SQLite Storage (7.2)

Implementing crate: `sqlite-storage`, [coverage report](../sqlite-storage/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **7.2.1** | Store and retrieve metadata from SQLite database | Done (via harness) |
| **7.2.2** | Configurable filesystem location for metadata database | Done |

### Storage and Recovery (7.3)

Implementing crates: `bpa`, storage backends, [storage harness](../tests/storage/docs/test_plan.md)

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **7.3.1** | Bundle and metadata storage selectable independently | Done | Storage harness runs one suite against every backend |
| **7.3.2** | Stored bundles resume after restart | Done | Storage harness META-12; `bpa/src/dispatcher/restart.rs` |
| **7.3.3** | Recovery reconciles the two stores | Done | Storage harness META-12, META-13, BLOB-04 |
| **7.3.4** | Recovery on operator request at start-up | Not Tested | `--recover-store` gates `Store::start`; no test drives the flag |
| **7.3.5** | Waiting limited by storage, not memory | Done | `bpa/src/storage/channel.rs::test_hysteresis_recovery`; storage harness META-06 |
| **7.3.6** | In-memory backend capacity and eviction | Done | `bpa/src/storage/bundle_mem.rs::test_eviction_policy_lru`, `test_min_bundles_protection`; `metadata_mem.rs::expired_tombstone_evicted_before_live` |
| **7.3.7** | BPA-level capacity and eviction order for any backend | Not implemented | — |
| **7.3.8** | No partial bundle treated as complete after a crash | Not implemented | `localdisk-storage`: `test_atomic_save_no_tmp_residue`, `test_failed_atomic_save_leaves_nothing_behind`; not yet for every backend |
| **7.3.9** | Automatic recovery after an unclean shutdown | Not implemented | — |
| **7.3.10** | Recovery reporting and check-only mode | Not implemented | — |

### S3 Storage (9.1)

Implementing crate: `s3-storage`, [coverage report](../s3-storage/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **9.1.1** | Configurable location and access credentials for S3 instance | Done |
| **9.1.2** | Configurable maximum total for stored bundle data on S3 | Withdrawn (see 7.3.7) |
| **9.1.3** | Configurable discard mechanism at S3 capacity | Withdrawn (see 7.3.7) |
| **9.1.4** | Use common S3 APIs (not implementor-specific) | Done |

Note: LLR 9.1.1 is verified by the storage-harness setup: every run supplies the endpoint and access credentials via configuration, and CI runs the suite against RustFS. LLR 9.1.4 is verified by design (uses `aws-sdk-s3`).

### Egress Scheduling (13.1)

Implementing crate: `bpa`

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **13.1.1** | Selectable scheduling policy | Done | `tests/pipeline.rs` custom `FlowControllerFactory` fixture |
| **13.1.2** | FIFO per peer by default | Done | `src/policy/mod.rs::null_policy_is_one_total_queue` |
| **13.1.3** | Scheduler never drops a bundle | Done | `src/storage/channel.rs::test_hysteresis_recovery` |
| **13.1.4** | Included fair-queuing policy | Not implemented | — |
| **13.1.5** | Traffic classes | Not implemented | — |
| **13.1.6** | TVR contact bandwidth applied as transmission rate | Not implemented | — |

### OpenTelemetry (19.1)

Implementing crate: `otel`, [coverage report](../otel/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **19.1.1** | Emit OpenTelemetry Log Records | Done |
| **19.1.2** | Emit OpenTelemetry Traces | Done |
| **19.1.3** | Emit OpenTelemetry Metrics | Done |

### Tools (19.2)

Implementing crate: `tools`, [coverage report](../tools/docs/test_coverage_report.md)

| LLR | Description | Status |
| :--- | :--- | :--- |
| **19.2.1** | Rate-controlled bundle sending tool | Done |
| **19.2.2** | Bundle reception response reporting tool | Done |
| **19.2.3** | Round-trip communication time reporting tool | Done |
| **19.2.4** | Tools do not rely on BPv7 status reports | Done |
| **19.2.5** | Tools run without requiring a local BPA | Done |

Note: LLR 19.2.2 is satisfied by the echo-service, which receives bundles and sends responses reported by `bp ping`.

### Process Integration APIs (18.2)

Implementing crates: `bpa`, `proto`. [proto coverage report](../proto/docs/test_coverage_report.md)

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **18.2.1** | CLAs, services and routing agents attachable over gRPC | Done | `proto` `app_cli_01..06`, `cla_cli_01..05`, `svc_cli_01..05`, `rte_cli_01..03` |
| **18.2.2** | Application and Service APIs | Done | `proto` `app_cli_*`, `svc_cli_*`; `bpa/tests/pipeline.rs` originate tests |
| **18.2.3** | Runtime registration and unregistration | Done | `proto` `life_01..06`; `bpa/tests/unregister_race.rs` |
| **18.2.4** | gRPC client authentication | Not implemented | [gRPC auth design](../bpa/docs/bpa-grpc-auth-design.md) |
| **18.2.5** | Components receive administrative records of their types | Not implemented | — |

### Framework APIs (23.1)

Implementing crates: every library crate. Verified by: inspection and CI

| LLR | Description | Status | Evidence |
| :--- | :--- | :--- | :--- |
| **23.1.1** | Integrator-supplied implementation of every extension point | Done | Verified by inspection of the `bpa` public API; exercised by the in-process fixtures in `bpa/tests/` |
| **23.1.2** | Integrators can do what the reference applications do | Done | `bpa-server`, `tcpclv4-server`, `tvr` and `tools` are separate crates depending on the published library crates; Rust visibility rules prevent private access |
| **23.1.3** | Libraries usable alone; include only what is used | Done | Per-crate `[features]`; CI builds the `no_std` crates with `--no-default-features` |
| **23.1.4** | Published to crates.io with RustDoc | Done | crates.io and docs.rs (see 21.2.2, 21.2.3) |
| **23.1.5** | Declared MSRV | Done | Workspace `rust-version`, inherited by every crate; CI `msrv` job in `.github/workflows/rust.yml` |
| **23.1.6** | Per-crate change log; Semantic Versioning | Done | Every crate has a `CHANGELOG.md` stating Semantic Versioning |
| **23.1.7** | Buildable example per extension point | Not implemented | Configuration examples exist (`bpa-server/examples`, `tvr/examples`); no code examples yet |
| **23.1.8** | Developer guide | Not implemented | — |

### Licence (21.1)

No LLRs. Verified by visual inspection (Apache 2.0 licence).

### Documentation (21.2)

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

### Embedded Platforms (22.1)

Verified by: the CI `no-std` job in `.github/workflows/rust.yml`

| LLR | Description | Status |
| :--- | :--- | :--- |
| **22.1.1** | cbor, bpv7, eid-patterns and btpu build `no_std` for `thumbv7em-none-eabihf` | Done |
| **22.1.2** | btpu builds for `thumbv6m-none-eabi` (no atomic compare-and-swap) | Done |
| **22.1.3** | bpa builds `no_std` for a bare-metal target | Not implemented |
| **22.1.4** | Embedded async runtime support | Not implemented |

### LLR Traceability Summary

| Section | Total | Done | Not Tested | Withdrawn | Not implemented |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **1.1** Standards Compliance | 1 | 1 | 0 | 0 | 0 |
| **1.1** CBOR Encoding | 4 | 4 | 0 | 0 | 0 |
| **1.1** CBOR Decoding | 6 | 6 | 0 | 0 | 0 |
| **1.1** CBOR General | 1 | 1 | 0 | 0 | 0 |
| **1.1** BPv7 Parsing | 11 | 10 | 0 | 1 | 0 |
| **1.1** BPv7 Bundle Generation | 5 | 5 | 0 | 0 | 0 |
| **1.1** BPv7 Bundle Processing | 5 | 5 | 0 | 0 | 0 |
| **1.2** Bundle Pipeline | 10 | 6 | 1 | 0 | 3 |
| **1.3** Bundle Filters | 7 | 5 | 0 | 0 | 2 |
| **2.1** BPSec | 16 | 9 | 1 | 0 | 6 |
| **2.2** RFC 9173 Security Contexts | 8 | 7 | 0 | 0 | 1 |
| **3.1** TCPCLv4 | 12 | 9 | 0 | 1 | 2 |
| **4.5** Bundle-in-Bundle Encapsulation | 3 | 1 | 0 | 0 | 2 |
| **5.6** BTP-U | 3 | 2 | 0 | 0 | 1 |
| **6.1** EID Patterns | 3 | 3 | 0 | 0 | 0 |
| **6.1** CLA APIs | 7 | 4 | 0 | 0 | 3 |
| **6.1** Routing | 11 | 9 | 0 | 0 | 2 |
| **7.1** Local Disk Storage | 3 | 1 | 0 | 2 | 0 |
| **7.2** SQLite Storage | 2 | 2 | 0 | 0 | 0 |
| **7.3** Storage and Recovery | 10 | 5 | 1 | 0 | 4 |
| **9.1** S3 Storage | 4 | 2 | 0 | 2 | 0 |
| **13.1** Egress Scheduling | 6 | 3 | 0 | 0 | 3 |
| **19.1** OpenTelemetry | 3 | 3 | 0 | 0 | 0 |
| **19.2** Tools | 5 | 5 | 0 | 0 | 0 |
| **18.2** Process Integration APIs | 5 | 3 | 0 | 0 | 2 |
| **23.1** Framework APIs | 8 | 6 | 0 | 0 | 2 |
| **21.2** Documentation | 3 | 3 | 0 | 0 | 0 |
| **21.3** Issue Reporting and Tracking | 2 | 2 | 0 | 0 | 0 |
| **22.1** Embedded Platforms | 4 | 2 | 0 | 0 | 2 |
| **Total** | **168** | **124** | **3** | **6** | **35** |

## 4. Gaps

### 4.1. Implemented but not yet traced to a test

| LLR | Gap |
| :--- | :--- |
| 1.2.4 | Forwarding and delivery status reports have no test asserting the report content. |
| 2.1.10 | No test asserts that `SecretBytes` redacts its `Debug` output. |
| 7.3.4 | No test drives `--recover-store`. |

### 4.2. Not Implemented

| Feature | Requirement | Notes |
| :--- | :--- | :--- |
| UDP Convergence Layer | REQ-4 | [UDPCLv2](https://datatracker.ietf.org/doc/draft-ietf-dtn-udpcl/) planned |
| BTP-U Convergence Layer | REQ-5, LLR 5.6.3 | Protocol engine done; no CLA yet |
| QUIC Convergence Layer | REQ-5 | [QUBICLE](https://datatracker.ietf.org/doc/draft-ek-dtn-qubicle/) planned |
| Custody Transfer | REQ-4 | Maintained outside the workspace ([extensions registry](extensions.md)) |
| Compressed Status Reporting | REQ-4 | Design doc exists |
| BP Managed Information | REQ-4 (4.12) | Also PICS item 28, below |
| BPSec COSE contexts | REQ-4 (4.11), LLR 2.1.16 | Once the IETF specification is stable |
| Contact bandwidth applied to forwarding | REQ-6 (6.7), LLR 13.1.6 | Bandwidth is parsed and carried; not enforced |
| Helm Charts | REQ-16 | Not started |
| Performance scale targets | REQ-13.2–13.6 | 4GB reassembly, 4GB streamed bundle, 1TB storage, 10Gbps TCPCLv4 |
| Soak test report | REQ-14 (14.2) | Scenarios defined, not run |
| Embedded bpa build | REQ-22, LLR 22.1.3 | bpa is `no_std` capable but not built for a bare-metal target in CI |

### 4.3. PICS Compliance Gap

| PICS Item | Feature | Status | Support | Impact |
| :--- | :--- | :--- | :--- | :--- |
| **28** | BP Managed Information (Annex C) | M | N | Only mandatory PICS item not implemented. See [PICS_Test_Mapping.md](PICS_Test_Mapping.md) §4.1. |

## 5. Conclusion

Core protocol compliance for RFC 9171 and RFC 9174 is satisfied; RFC 9172/9173 compliance (REQ-2) is Partial, because the BPA does not yet act as a BPSec security source at egress (LLR 2.1.11) and several RFC 9172 policy features are not implemented. REQ-5 (experimental convergence layers) is Partial: the BTP-U protocol engine is complete and traced to its draft, but there is no BTP-U convergence layer adaptor yet, and QUIC is not started. REQ-6 is Partial because contact bandwidth is recorded but not applied to forwarding. REQ-23 (framework APIs) is Partial: the extension points are public, versioned and published, but code examples and a developer guide are still to come. Of 168 LLRs, 124 are Done, 3 are implemented but not yet traced to a test, 6 are withdrawn, and 35 are not implemented. REQ-20 (interoperability) is Partial: 7 of the 8 listed implementations are verified, and Unibo-BP has no interop harness yet.
