# Requirements Traceability Matrix (RTM)

| **Document Info** | **Details** |
| :--- | :--- |
| **Project** | Hardy (Cloud-based DTN Router) |
| **Version** | 2.0 (Complete) |
| **Sources** | `DTN-HLR_High-level requirements_v1` (HLR)<br>`DTN-LLR_Low-level requirements_v1.1` (LLR) |

## 1. High-Level Requirements (HLR)

| **ID** | **Category** | **Criticality** | **Description** |
| :--- | :--- | :--- | :--- |
| **REQ-1** | Compliance | Critical | Full compliance with RFC 9171 (Bundle Protocol Version 7). |
| **REQ-2** | Security | Critical | Full compliance with RFC 9172 (BPSec) and RFC 9173 (Security Contexts). |
| **REQ-3** | Transport | Critical | Full compliance with RFC 9174 (TCPCLv4). |
| **REQ-4** | Standards | Desirable | Alignment with on-going DTN standardisation (UDP-CL, Custody Transfer, QoS). |
| **REQ-5** | Transport | Optional | Experimental support for QUIC as a convergence layer. |
| **REQ-6** | Routing | Critical | Time-variant Routing API to allow real-time configuration of contacts and bandwidth. |
| **REQ-7** | Storage | Critical | Support for local filesystem for bundle and metadata storage. |
| **REQ-8** | Storage | Desirable | Support for PostgreSQL for bundle metadata storage. |
| **REQ-9** | Storage | Desirable | Support for Amazon S3 storage for bundle storage. |
| **REQ-10** | Storage | Desirable | Support for Amazon DynamoDB for bundle metadata storage. |
| **REQ-11** | Storage | Desirable | Support for Azure Blob Storage for bundle storage. |
| **REQ-12** | Storage | Desirable | Support for Azure SQL for bundle metadata storage. |
| **REQ-13** | Performance | Critical | Throughput > 1000 bundles/sec; Reassembly > 4GB; Storage > 1TB; 10Gbps Data Rates. |
| **REQ-14** | Reliability | Critical | Published MTBF figures; Fuzz testing of all external APIs. |
| **REQ-15** | Packaging | Critical | Independent component packaging (OCI Containers). |
| **REQ-16** | Deployment | Desirable | Kubernetes packaging (Helm Charts) for EKS, AKS, and generic K8s. |
| **REQ-17** | Docs | Critical | Comprehensive usage documentation (Installation, Configuration). |
| **REQ-18** | Docs | Critical | Comprehensive technical documentation and examples (APIs, SDKs). |
| **REQ-19** | Ops | Critical | A well-featured suite of management and monitoring tools (OpenTelemetry). |
| **REQ-20** | Interop | Desirable | Interoperability testing with existing implementations (ION, HDTN, DTNME, cFS). |
| **REQ-21** | Legal | Critical | Available to all under a permissive open-source licence. |

## 2. Low-Level Requirements (LLR)

### 2.1 Standards Compliance & CBOR (Parent: REQ-1)

| **ID** | **Description** |
| :--- | :--- |
| **1.1.1** | Compliant with all mandatory requirements of CCSDS Bundle Protocol (CCSDS 734.20-O-1). |
| **1.1.2** | CBOR encoder must support explicit emission of tagged and untagged data types. |
| **1.1.3** | CBOR encoder must support all major types defined in RFC 8949 Sec 3.1. |
| **1.1.4** | CBOR encoder must emit all primitive types in canonical form (RFC 8949 Sec 4.2). |
| **1.1.5** | CBOR encoder must ensure Maps/Arrays have correct item counts for definite length. |
| **1.1.6** | CBOR encoder must indicate total bytes and offset of primitive data items. |
| **1.1.7** | CBOR decoder must report if a parsed data item is in canonical form. |
| **1.1.8** | CBOR decoder must report if a parsed data item has associated tags. |
| **1.1.9** | CBOR decoder must support all primitive data items defined in specification. |
| **1.1.10** | CBOR decoder must parse items within context of Maps/Arrays correctly. |
| **1.1.11** | CBOR decoder must support opportunistic parsing (try-parse). |
| **1.1.12** | CBOR decoder must indicate if an incomplete item is found at end of buffer. |
| **1.1.13** | CBOR processing must be suitable for embedded platforms (`no_std`). |

### 2.2 BPv7 Parsing & Generation (Parent: REQ-1)

| **ID** | **Description** |
| :--- | :--- |
| **1.1.14** | Parser must indicate when bundle rewriting has occurred. |
| **1.1.15** | Parser must indicate that the Primary Block is valid. |
| **1.1.16** | Parser must indicate that all recognised extension blocks are valid. |
| **1.1.17** | Parser must indicate that the Bundle as a whole is valid. |
| **1.1.18** | Parser must not fail when presented with unrecognised but correctly encoded flags. |
| **1.1.19** | Parser must parse/validate extension blocks specified in RFC 9171. |
| **1.1.20** | Parser should parse/validate extension blocks when content spec is available. |
| **1.1.21** | Parser must parse and validate all CRC values. |
| **1.1.22** | Parser must support all CRC types specified in RFC 9171. |
| **1.1.23** | Parser must support 3-element CBOR encoding of `ipn` scheme EIDs (RFC 9758). |
| **1.1.24** | Parser should indicate if `ipn` EID used 2-element (legacy) or 3-element encoding. |
| **1.1.25** | Generator must create valid, canonical CBOR encoded bundles. |
| **1.1.26** | Generator must only include valid, canonical extension blocks. |
| **1.1.27** | Generator must apply required CRC values to all bundles. |
| **1.1.28** | Generator must apply required CRC values to all extension blocks. |
| **1.1.29** | Generator must allow caller to specify the CRC type (16/32/None). |
| **1.1.30** | Processing must enforce bundle rewriting rules when discarding unrecognised blocks. |
| **1.1.31** | Processing may rewrite non-canonical bundles into canonical form (policy allow). |
| **1.1.32** | Parser must indicate reason for rewriting to security policy. |
| **1.1.33** | Processing must use Bundle Age block for expiry if Creation Time is zero. |
| **1.1.34** | Processing must process and act on Hop Count extension block. |

### 2.3 BPSec & Security (Parent: REQ-2)

| **ID** | **Description** |
| :--- | :--- |
| **2.1.1** | Validate BIB/BCB blocks according to abstract syntax (RFC 9172). |
| **2.1.2** | Correctly remove BPSec target info when targeted block is removed. |
| **2.1.3** | Validate that Fragmented bundles do NOT contain BPSec extension blocks. |
| **2.2.1** | Support BIB-HMAC-SHA2 context with 256-bit hash. |
| **2.2.2** | Support BIB-HMAC-SHA2 context with 384-bit hash. |
| **2.2.3** | Support BIB-HMAC-SHA2 context with 512-bit hash. |
| **2.2.4** | Support key-wrap function on HMAC keys. |
| **2.2.5** | Support BCB-AES-GCM context with 128-bit key. |
| **2.2.6** | Support BCB-AES-GCM context with 256-bit key. |
| **2.2.7** | Support key-wrap function on AES keys. |

### 2.4 TCPCLv4 (Parent: REQ-3)

| **ID** | **Description** |
| :--- | :--- |
| **3.1.1** | Support 'Active' session establishment. |
| **3.1.2** | Support 'Passive' session establishment. |
| **3.1.3** | Maintain a pool of idle connections for reuse. |
| **3.1.4** | Provide local node IDs in Session Init (RFC 9174 Sec 4.6). |
| **3.1.5** | Allow configuration of default session parameters (Keepalive, Segment Size). |
| **3.1.6** | Correctly process extension items in Session Init. |
| **3.1.7** | Support TLS (RFC 9174 Sec 4.4). |
| **3.1.8** | Default to using TLS unless explicitly disabled. |
| **3.1.9** | Support TLS Entity ID using DNS Name and Network Address methods. |
| **3.1.10** | Support Session Upkeep (Keepalive) messages. |

### 2.5 Routing & APIs (Parent: REQ-6)

| **ID** | **Description** |
| :--- | :--- |
| **6.1.1** | Correctly parse textual representation of `ipn` and `dtn` EID patterns. |
| **6.1.2** | Provide function to match a specific EID against a Pattern. |
| **6.1.3** | Provide API for CLAs to indicate success of forwarding. |
| **6.1.4** | Provide API for EID resolution to CLA addresses (e.g., DNS for TCPCL). |
| **6.1.5** | Allow administrator to specify routing info via config file. |
| **6.1.6** | Provide API to add/remove routes at runtime. |
| **6.1.7** | Provide ability to discard bundles based on destination. |
| **6.1.8** | Provide ability to reflect bundle back to previous node. |
| **6.1.9** | Provide mechanism to prioritise routing rules. |
| **6.1.10** | Implement Equal Cost Multi-Path (ECMP). |

### 2.6 Storage (Parent: REQ-7, REQ-9)

| **ID** | **Description** |
| :--- | :--- |
| **7.1.1** | Configurable location for Local Disk bundle storage. |
| **7.1.2** | Configurable maximum total size for Local Disk storage. |
| **7.1.3** | Configurable discard policy (FIFO/Priority) when storage full. |
| **7.2.1** | Store/Retrieve metadata using SQLite. |
| **7.2.2** | Configurable filesystem location for SQLite database. |
| **9.1.1** | Configure location/credentials for S3 storage instance. |
| **9.1.2** | Configurable maximum total for S3 storage. |
| **9.1.3** | Configurable discard policy for S3 storage. |
