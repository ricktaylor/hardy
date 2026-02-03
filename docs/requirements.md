# Hardy Requirements Specification

## Document Information

| **Field** | **Value** |
| :--- | :--- |
| **Project** | Hardy (Cloud-based DTN Router for Ground Systems) |
| **Version** | 3.0 (Consolidated) |

## Scope

Hardy is focussed on the terrestrial elements of a heterogeneous Bundle Protocol DTN. The interface between the core functionality and specific communication systems at ground stations is provided via Convergence Layer adaptors designed for Internet usage: initially TCPCLv4 (REQ-3), with experimental support for QUIC as a convergence layer transport protocol (REQ-5).

Interoperability testing with existing Bundle Protocol implementations with radio-layer specific convergence layer adaptors, such as LTP over AOS, or CCSDS Bundle-in-EPP, ensures that support for specific link-layer technology can be integrated without modification of either implementation.

---

# Part 1: Technical Objectives

The technical objectives of Hardy are to deliver a reliable, performant, secure, interoperable, extensible Bundle Protocol implementation; with the cloud as a primary target environment, accompanied by high-quality documentation, examples, and tools, made available to all under a permissive licence.

## Objective 1: A reliable, performant, secure, interoperable, extensible Bundle Protocol implementation

The core purpose of the solution is to act as a node within a larger delay-tolerant network.

As a component within such a network, the critical capabilities of a particular node are to:

1. Successfully connect to the network.
2. Remain a member of the network without aborting or misbehaving.
3. Move data across the network, without loss, in a secure and timely manner.

The solution must have those capabilities, or it cannot be considered fit for purpose.

As Hardy focuses on just the ground segment of a heterogeneous DTN, rather than a bespoke space-to-ground DTN solution, the ability to interoperate with other system-of-systems components is vital. Therefore open and de-facto standards are implemented to ensure the widest possible interoperability. The standard selected is the Bundle Protocol Version 7, published by the IETF, and documented, extended, and profiled by the CCSDS. Supporting this standard is a key requirement of compliance with the LunaNet Interoperability Specification.

In order to meet the reliability, performance and security objective, the choice of programming language is key. The solution is implemented in Rust (<https://www.rust-lang.org/>), a compiled, memory-safe, systems-programming language renowned for its reliability, performance, and security. Flight software is almost always required to be implemented in C or ADA, however as there is no spaceflight requirement on the solution a more modern alternative was selected, reducing the time to deliver without compromising performance or correctness. There is increased interest in the automotive industry in Rust for safety critical software, supporting this decision on its suitability.

Where possible, every component is implemented in 'async' Rust (<https://rust-lang.github.io/async-book/>), a state-of-the-art technology maximising the use of multiprocessor platforms by automatically spreading packet processing load across all available cores. Cloud service providers offer compute platforms with a very large number of cores, and ensuring full utilisation of these resources maximises performance.

## Objective 2: The Cloud as the target environment

Given one of the key differentiators for the solution is its focus on the Cloud as a target environment, it must take advantage of the capabilities of that environment to maximum effect. There is a difference between software that can run in the Cloud, and software that is truly 'Cloud native': the former can survive, the latter can flourish.

One of the major advantages of deploying applications to the Cloud is the ability to scale instances of services, providing increased reliability through redundancy, and increased performance through load balancing. To leverage this capability, the solution is architected as a set of independently deployable containerized microservices, interconnected via the Cloud-provider high-speed network.

Although Cloud service providers support virtualized disks for applications to use for storage, they also provide managed storage services at considerably better capacity, reliability, and performance price-points, but only if applications support their APIs. Given a Bundle Protocol Agent needs to store both the payload and the metadata describing the payload, the solution is modular with respect to its use of storage: a "plug-in" model provides the common bundle processing capability with the ability to use native Cloud data storage services for both metadata and payload, configurable at time of deployment.

The three Cloud environments targeted are:

- **Generic capability**: covering support for common technology available across the majority of cloud service providers, but not tuned for any in particular.
- **Amazon Web Services**: the Cloud offering from Amazon.
- **Microsoft Azure**: the Cloud service from Microsoft.

The specialisation of the solution for the services from Amazon and Microsoft was decided because Amazon dominates the Cloud service provider market, and Microsoft have 'private' Cloud service offerings specifically tuned for the needs of the government sector. Specialisation for Google Cloud services, for example, can be added reasonably quickly after the completion of the full development.

## Objective 3: High-quality documentation, examples, and tools, available to all

Beyond the list of functional capabilities, there are operational aspects that are just as important to the success of the solution: No matter how well-featured, if the solution is almost impossible to install, configure, manage, and monitor, either in isolation or as part of a larger system, then it is essentially useless.

To that end, the solution is supported by well-written documentation that describes not only the general capabilities and usage of the solution, but also how to configure, extend and integrate into a larger system.

To provide operators with the ability to monitor and manage any deployment, the solution supports modern Cloud-native observability tools, and provides a suite of diagnostic tools to aid the deployment and monitoring of the solution.

---

# Part 2: High-Level Requirements (HLR)

In order to fulfil these objectives, Hardy meets the following high-level technical requirements. Each high-level requirement is broken down into a series of atomic low-level requirements suitable for continuous testing, verification and validation.

## REQ-1: Full compliance with RFC9171

The IETF Proposed Standard: "RFC 9171 Bundle Protocol Version 7" (<https://www.rfc-editor.org/rfc/rfc9171>) specifies the expected behaviour of compliant Bundle Protocol agent implementations, and is the target baseline for the functionality of the solution.

The solution targets 100% compliance. This is confirmed through automated testing. Every requirement of the specification has a corresponding unit or component test that asserts the correct behaviour. A verification matrix maps every requirement of the specification to a pass/fail. Optional parts of the specification that are not supported have a corresponding statement justifying the lack of support, e.g. "Not appropriate for the target environment".

*This requirement supports Objective 1.*

## REQ-2: Full compliance with RFC9172 and RFC9173

The IETF Proposed Standard: "RFC 9172 Bundle Protocol Security (BPSec)" (<https://www.rfc-editor.org/rfc/rfc9172>) specifies the mechanisms and required behaviours of a Bundle Protocol agent implementation in order to provide end-to-end integrity and confidentiality of data in transit and at rest.

The IETF Proposed Standard: "RFC 9173 Default Security Contexts for Bundle Protocol Security (BPSec)" (<https://datatracker.ietf.org/doc/rfc9173/>) defines a set of cryptographic primitives and configuration options for BPSec that acts a baseline requirement for interoperability between implementations.

The solution targets 100% compliance. This is confirmed through automated testing. Every requirement of the specifications has a corresponding unit or component test that asserts the correct behaviour. A verification matrix maps every requirement of the specifications to a pass/fail. Optional parts of the specifications that are not supported have a corresponding statement justifying the lack of support.

*This requirement supports Objective 1.*

## REQ-3: Full compliance with RFC9174

The IETF Proposed Standard: "RFC 9174 Delay-Tolerant Networking TCP Convergence-Layer Protocol Version 4" (TCP-CL) (<https://www.rfc-editor.org/rfc/rfc9174>) specifies the expected behaviour of Bundle Protocol agent implementations that wish to intercommunicate using TCP/IP. In the Bundle Protocol architecture, "convergence layers" are the abstract specifications of the link-layer over which bundles are passed between nodes, and TCP-CL is specified as a baseline for interoperability.

The solution targets 100% compliance. This is confirmed through automated testing. Every requirement of the specification has a corresponding unit or component test that asserts the correct behaviour. A verification matrix maps every requirement of the specification to a pass/fail.

*This requirement supports Objective 1.*

## REQ-4: Alignment with on-going DTN standardisation

There is on-going work in both CCSDS and the IETF to develop and standardise extensions to the base Bundle Protocol version 7, including:

- UDP convergence layer adaptor
- Custody Transfer and end-to-end reliability
- Quality of Service and service flow marking
- Compressed Status Reporting
- Bundle-in-Bundle Encapsulation

The solution is designed to be extensible in order to support development of these and other future extensions, and act as a suitable testbed for the evaluation at scale of such extensions.

This requirement is a goal of the solution, as the final outcome cannot be clearly specified. A support matrix maps the capabilities of pre-standard extensions to the capabilities of the solution implementation, indicating either support, or noting the reasons for any absence. Verification of capability is performed by automated unit or component testing. Use of extensions is configurable.

*This requirement supports Objective 1.*

## REQ-5: Experimental support for QUIC

There is currently active research in the deep space community concerning the applicability of recent transport protocol developments for the Internet to deep-space uses-cases. One primary candidate is the IETF Proposed Standard "RFC 9000 QUIC: A UDP-Based Multiplexed and Secure Transport". The QUIC protocol may deliver performance and scalability advantages over TCP-CL for the terrestrial segment of a DTN, and in order to assist the progress of experimentation and standardisation of QUIC for deep-space use-cases, the solution shall include experimental support for QUIC as a Convergence Layer protocol.

This requirement is a goal of the solution, as the final outcome cannot be clearly specified. A support matrix maps the capabilities of the QUIC protocol to the capabilities of the convergence layer implementation, indicating either support, or noting the reasons for any absence. Verification of capability is performed by automated unit or component testing. Use of QUIC functionality is configurable.

*This requirement supports Objective 1.*

## REQ-6: Time-variant Routing API

A DTN node without the ability to forward bundles to adjacent nodes is essentially useless. In order to understand the availability and reachability of suitable nodes for bundle forwarding, a facility shall be implemented to allow such configuration.

A number of mechanisms exist to calculate the 'contact graph' for the transmission opportunities for subnets of an entire DTN, including the CCSDS defined specification: Schedule-Aware Bundle Routing (SABR) (<https://public.ccsds.org/Pubs/734x3b1.pdf>). Additionally, temporospatial orchestration systems can produce similar realtime contact graphs.

The solution shall support both (and other) such contact graph calculation systems by enabling the relevant sub-graphs of the greater contact graph to be applied to the store and forward policies of deployed nodes via a common Time-variant Routing API.

Explicitly, the solution shall provide an API to allow the real-time configuration of:

- Scheduled adjacent node contact time, including start of contact and duration.
- Expected periodicity of contact, e.g. every 27.5 hours.
- Available bandwidth during contact, e.g. 128 kbits/sec.

The store and forward configuration shall be updatable during deployment, without requiring a restart, allowing the routing information to be reconfigured by an external process without incurring an interruption to service.

The solution targets 100% compliance. This is confirmed through automated testing. This requirement is validated via a verification matrix mapping required solution functionality to pass/fail.

*This requirement supports Objective 1.*

## REQ-7: Support for local filesystem for bundle and metadata storage

Although the primary target environment for the solution is the Cloud, the ability to deploy to a single server in a 'bare-metal' configuration is a required capability. The solution shall support the ability to use the local operating system filesystem as a storage location for bundles and associated metadata.

The solution targets 100% compliance. This is confirmed through automated testing. Tests not only assert correct usage of the local filesystem, but also the achievable level of reliability offered by the filesystem. This requirement is validated via a verification matrix mapping required solution functionality to pass/fail. A reliability report is also produced, cross-referencing failure modes to chance of data loss, qualified by supported filesystem format: e.g. application abort, physical disk failure, and system power reset.

*This requirement supports Objective 1.*

## REQ-8: Support for PostgreSQL for bundle metadata storage

Beyond the ability to store bundle metadata on the local filesystem, there are deployment efficiency and reliability advantages to sharing the metadata storage requirements of several bundle agents with a common relational database engine. The solution shall provide this capability by supporting the PostgreSQL database engine.

The major Cloud service providers all provide a managed PostgreSQL solution, and local installation for non-cloud deployments is simple and well-documented.

The solution targets 100% compliance. This is confirmed through automated testing. Tests not only assert correct usage of PostgreSQL, but also the achievable level of reliability. This requirement is validated via a verification matrix mapping required solution functionality to pass/fail. A reliability report is also produced, cross-referencing failure modes to chance of data loss.

*This requirement supports Objectives 1 and 2.*

## REQ-9: Support for Amazon S3 storage for bundle storage

Amazon Web Services (AWS) provide an integrated Cloud storage solution for large binary data: Amazon Simple Storage Service (S3).

As one of the de-facto standards in cloud storage solutions, the S3 API is supported by many of the other Cloud service providers, and the ability to directly interface with this kind of storage is a core focus of the solution. Therefore the solution shall support the use of S3 for the storage of bundles.

The solution targets 100% compliance. This is confirmed through automated testing. Tests not only assert correct usage of the S3 API, but also the achievable level of reliability. This requirement is validated via a verification matrix mapping required solution functionality to pass/fail. A reliability report is also produced, cross-referencing failure modes to chance of data loss.

*This requirement supports Objectives 1 and 2.*

## REQ-10: Support for Amazon DynamoDB for bundle metadata storage

Amazon Web Services (AWS) provides an integrated Cloud storage solution for non-relational (NoSQL) indexed key-value data: Amazon DynamoDB.

The storage of bundle metadata does not require the full capabilities of a relational database, and a simpler key-value data store can provide a cost and performance improvement. Therefore the solution shall support the use of Amazon DynamoDB for the storage of bundle metadata.

The solution targets 100% compliance. This is confirmed through automated testing. Tests not only assert correct usage of the DynamoDB API, but also the achievable level of reliability.

*This requirement supports Objectives 1 and 2.*

## REQ-11: Support for Azure Blob Storage for bundle storage

Azure Blob Storage is the integrated Cloud storage solution for large binary data from Microsoft.

Microsoft has a long history of providing Cloud services to government, defence and enterprise markets, and the ability to directly interface with their storage APIs is a core focus of the solution. Therefore the solution shall support the use of Azure Blob Storage for the storage of bundles.

The solution targets 100% compliance. This is confirmed through automated testing. Tests not only assert correct usage of the API, but also the achievable level of reliability.

*This requirement supports Objectives 1 and 2.*

## REQ-12: Support for Azure SQL for bundle metadata storage

Microsoft provides a relational database solution as part of their managed Cloud services: Azure SQL, built on the enterprise-grade Microsoft SQL Server. In order to support a full integration with a Microsoft provided Cloud service, the solution shall support Azure SQL as a storage option for bundle metadata.

The solution targets 100% compliance. This is confirmed through automated testing. Tests not only assert correct usage of Azure SQL, but also the achievable level of reliability.

*This requirement supports Objectives 1 and 2.*

## REQ-13: Performance

Given the target of the solution is the ground segment, no differentiation between 'uplink' or 'downlink' is made. Peak performance of the solution is dependent on the underlying capabilities of the network and network interfaces.

The BPA component shall be capable of supporting, at minimum:

- The processing of 1000 bundles per second.
- The fragmentation and reassembly of bundles with a total reassembled size of at least 4GB.
- The storage of at least 1TB of bundles at rest, for 1 month.

The TCP-CL convergence layer adaptor shall be capable of supporting, at minimum:

- 10Gbit/s data rates, with the following combinations of bundle, CL segment size, and link-layer maximum transmittable unit (MTU) sizes:
  - 10MB bundle size, 16kB segment size, 1350 byte link-layer MTU
  - 1kB bundle size, 1kB segment size, 576 byte link-layer MTU

The solution targets 100% compliance. This is confirmed through automated testing. This requirement is validated via a verification matrix mapping required performance to the achieved result, broken down by convergence-layer and target environment configuration.

*Note: Additional performance can be achieved by increasing the size of the underlying virtual machine, and/or scaling out the number of instances of the BPA and TCP-CL, but the requirements above set the baseline for a single instance with a single network interface.*

*This requirement supports Objective 1.*

## REQ-14: Reliability

The reliability of the solution is calculated during development and testing, with a goal of maximising reliability. Each independent component has a verifiable, published Mean Time Between Failure (MTBF) figure. This allows the MTBF of the solution as a whole to be calculated. These figures are determined by soak-testing the solution with representative data, possibly derived from the data used for performance validation.

Additionally, every internal and external API is checked for correctness with unit, component, or 'fuzz' testing, to ensure that incorrect or malicious input does not cause any component to abort or become compromised.

*This requirement supports Objective 1.*

## REQ-15: Independent component packaging

In order to simplify the installation and management of the independent components of the solution, each is packaged as a container image in a manner compliant with the Open Container Initiative standards on packaging (<https://opencontainers.org/>), with complete supporting documentation.

The solution targets 100% compliance. This is confirmed through automated testing of the packaging, and visual inspection of the documentation. This requirement is validated via a verification matrix mapping the ability to install and uninstall each component to a pass/fail per target environment.

*This requirement supports Objectives 2 and 3.*

## REQ-16: Kubernetes packaging

The de-facto standard manner of deploying a software solution made of multiple containerised components is to use the Kubernetes container orchestration platform (<https://kubernetes.io/>). All the major Cloud infrastructure providers have a managed Kubernetes service. Installation onto on-premise servers is well supported, and the ability to leverage this technology is key to the success of the solution.

Hardy shall include suitable Kubernetes deployment 'charts', and supporting documentation, enabling the simple deployment of the solution to the following Kubernetes environments:

- Amazon Elastic Kubernetes Service (EKS) (<https://aws.amazon.com/eks/>)
- Microsoft Azure Kubernetes Service (AKS) (<https://azure.microsoft.com/en-gb/products/kubernetes-service>)
- A self-hosted or 'vanilla' Kubernetes installation, for example MiniKube (<https://minikube.sigs.k8s.io/docs/>)

The solution targets 100% compliance. This is confirmed through automated testing of the charts, and visual inspection of the documentation. This requirement is validated via a verification matrix mapping the ability to install and uninstall to a pass/fail per target environment.

*This requirement supports Objectives 2 and 3.*

## REQ-17: Comprehensive usage documentation

Just like correctly deploying and configuring an IP router, successfully deploying a BPv7 bundle agent can be non-trivial. Each independently configurable component of the solution has a potentially large number of options that may have a major impact on the capability of the solution as a whole. Correctly configuring the solution is critical to maximising performance or security for a particular deployment.

Therefore the solution is accompanied by complete usage documentation. The documentation includes a general overview of the capabilities and usage of the solution, as well as accurate technical information concerning the configuration options available.

In order to keep pace with the rapid rate of development, documentation is published in a publicly accessible digital form, and kept up to date with each release.

The solution targets 100% compliance. This is confirmed by visual inspection. This requirement is validated via a verification matrix mapping each configurable option of every component to a corresponding section of the documentation.

*This requirement supports Objective 3.*

## REQ-18: Comprehensive technical documentation and examples

Although usable stand-alone, the solution is also designed to be part of a larger system-of-systems, exposing APIs allowing third-parties to extend the platform, and integrate with existing ground-segment capabilities.

In order to make extending the solution as painless as possible, the solution develops all external APIs using the de-facto gRPC standard (<https://grpc.io/>) allowing development of extensions to be performed in the widest range of languages. Accompanying all API specifications is complete documentation of the correct usage of APIs, and documented example source code.

The solution targets 100% compliance. This is confirmed by visual inspection. This requirement is validated via a verification matrix mapping each API function of every external interface to a corresponding section of the documentation and example.

*This requirement supports Objectives 1 and 3.*

## REQ-19: A well-featured suite of management and monitoring tools

To support deployment and monitoring, the solution integrates with the OpenTelemetry API standards (<https://opentelemetry.io/>), and includes documentation and configuration examples. The OpenTelemetry APIs allow the distribution of logs and performance traces suitable for the management and monitoring of complex distributed services such as this solution, and is the de-facto standard for software deployed to the Cloud.

Additionally a suite of simple management and monitoring tools, such as "BP-trace", and including all bespoke tools developed for the verification of system requirements is included, with supporting documentation, alongside the other deliverables of the project.

This requirement is a goal of the solution, as the exact nature and number of the tools is determined during the development process. This is confirmed by visual inspection. This requirement is validated via a verification matrix mapping each delivered tool function to a corresponding section of the documentation.

*Note: Although management protocols for DTNs exist, such as IETF DTN-MA, these protocols are designed to support the autonomous management and monitoring of remote agents via a DTN. Hardy is designed to be deployed in a terrestrial Cloud environment where the needs for management via the DTN are not required, hence DTN-MA is out of scope.*

*This requirement supports Objective 3.*

## REQ-20: Interoperability with existing implementations

The solution is not designed to be deployed in isolation, but instead viewed as a component within a much larger heterogeneous delay-tolerant space network. Therefore interoperability with existing implementations, including the required configuration, must be known and documented.

To that end, the solution includes interoperability testing with the following popular, full-featured DTN implementations:

- ION (NASA/JPL)
- HDTN (NASA/Glenn)
- DTNME (NASA/Marshal)
- cFS (NASA/Goddard)
- μD3TN (D3TN GmbH)
- The ESA BP Implementation

This requirement is a goal of the solution, as access to functional versions of the above implementations may not always be possible. A compatibility matrix is published mapping the BPv7 capabilities of each implementation above to the interoperable capability of the solution, including the required configuration of both implementations and the target environment. This is confirmed by automated testing. This requirement is validated via a verification matrix mapping each BPv7 function to a corresponding interoperability statement and test result.

*This requirement supports Objective 1.*

## REQ-21: Available to all under a permissive licence

In order to maximise the availability of the solution to all, the solution is licensed and made available under a permissive open-source licence.

This is verified by inspection.

*This requirement supports Objective 3.*

---

# Part 3: Low-Level Requirements (LLR)

This document introduces additional Low-level Requirements, derived from the High- and Mid-level requirements specified in Part 2.

Not all high- or mid-level requirements have derived requirements, only those where additional specificity is required.

All requirements detailed here are validated through automated tests, and verified by examination of test reports.

## 3.1 Standards Compliance (Parent: REQ-1)

| ID | Description |
| :--- | :--- |
| **1.1.1** | The implementation shall be compliant with all mandatory requirements of the CCSDS Bundle Protocol Specification (CCSDS 734.20-O-1), excluding the ADU Fragmentation process as defined in Section 5.8 of RFC 9171 |

## 3.2 CBOR Encoding (Parent: REQ-1)

| ID | Description |
| :--- | :--- |
| **1.1.2** | The CBOR encoding functionality must support the explicit emission of tagged and untagged data types, to ensure deterministically encoded CBOR, section 4.2 of RFC 8949, can be parsed appropriately |
| **1.1.3** | The CBOR encoding functionality must support all the major types as defined in section 3.1 of RFC 8949 |
| **1.1.4** | The CBOR encoding functionality must emit all primitive types in canonical form, to ensure deterministically encoded CBOR, section 4.2 of RFC 8949, can be used appropriately |
| **1.1.5** | The CBOR encoding functionality must ensure that all Maps and Arrays have the correct number of data items for definite length sequences |

## 3.3 CBOR Decoding (Parent: REQ-1)

| ID | Description |
| :--- | :--- |
| **1.1.7** | The CBOR decoding functionality must report if a parsed data item is in canonical form |
| **1.1.8** | The CBOR decoding functionality must report if a parsed data item has any associated tags |
| **1.1.9** | The CBOR decoding functionality must support all the primitive data items defined in the CBOR specification |
| **1.1.10** | The CBOR decoding functionality must ensure that data items contained within the serialization of a CBOR Map or Array are parsed within the context of the relevant sequence correctly |
| **1.1.11** | The CBOR decoding functionality must support the ability to opportunistically parse an item from a byte sequence, and only return a data item if one exists |
| **1.1.12** | The CBOR decoding functionality must indicate if an incomplete data item is found at the end of a byte sequence, which could be correctly parsed if more bytes were to be provided |

## 3.4 CBOR General (Parent: REQ-1)

| ID | Description |
| :--- | :--- |
| **1.1.13** | The CBOR processing functionality should be suitable for use in embedded platforms (`no_std`) |

## 3.5 BPv7 Parsing (Parent: REQ-1)

| ID | Description |
| :--- | :--- |
| **1.1.14** | The bundle parsing functionality may support the rewriting of bundles during parsing for performance reasons. If so it must indicate when such bundle rewriting has occurred |
| **1.1.15** | The bundle parsing functionality must indicate that the primary block is valid |
| **1.1.16** | The bundle parsing functionality must indicate that all recognised extension blocks are valid |
| **1.1.17** | The bundle parsing functionality must indicate that a bundle is valid |
| **1.1.18** | The bundle parsing functionality must not fail when presented with unrecognised but correctly encoded flags or type identifiers |
| **1.1.19** | The bundle parsing functionality must parse and validate any extension blocks specified in RFC9171 |
| **1.1.20** | The bundle parsing functionality should parse and validate any extension blocks when a specification of the content is available |
| **1.1.21** | The bundle parsing functionality must parse and validate all CRC values |
| **1.1.22** | The bundle parsing functionality must support all CRC types specified in RFC9171 |
| **1.1.23** | The bundle parsing functionality must support the 3-element CBOR encoding of 'ipn' scheme EIDs as defined in RFC 9758 |
| **1.1.24** | The bundle parsing functionality should indicate if an 'ipn' URI scheme EID has been parsed from the 3-element CBOR encoding, or the legacy 2-element packed CBOR encoding |

## 3.6 BPv7 Bundle Generation (Parent: REQ-1)

| ID | Description |
| :--- | :--- |
| **1.1.25** | The bundle creation and rewriting functionality must generate valid, canonical CBOR encoded bundles |
| **1.1.26** | The bundle creation and rewriting functionality must only include valid, canonical CBOR encoded extension blocks as part of a bundle |
| **1.1.27** | The bundle creation and rewriting functionality must apply the required CRC values to all generated bundles |
| **1.1.28** | The bundle creation and rewriting functionality must apply the required CRC values to all generated extension blocks |
| **1.1.29** | The bundle creation functionality must allow a caller to specify the CRC type to be applied to the new bundle |

## 3.7 BPv7 Bundle Processing (Parent: REQ-1)

| ID | Description |
| :--- | :--- |
| **1.1.30** | The bundle processing functionality must enforce the bundle rewriting rules when discarding unrecognised extension blocks |
| **1.1.31** | The bundle processing functionality may rewrite bundles that use non-canonical encodings into a canonical form when allowed by policy |
| **1.1.32** | The bundle parsing functionality must indicate the reason for any rewriting such that a security policy enforcing function can determine the correct action to take |
| **1.1.33** | The bundle processing functionality must recognise and process the Bundle Age extension block when determining if a bundle's lifetime has expired |
| **1.1.34** | The bundle processing functionality must process and act on the Hop Count extension block |

## 3.8 BPSec (Parent: REQ-2) - *Optional for initial development*

| ID | Description |
| :--- | :--- |
| **2.1.1** | The bundle parsing functionality shall validate BPSec integrity and confidentiality blocks for validity according to the abstract block syntax as specified in RFC 9172 |
| **2.1.2** | The bundle parsing functionality shall correctly remove BPSec target information from any affected confidentiality or integrity block, when the targeted block is removed |
| **2.1.3** | The bundle parsing functionality shall validate that any bundle with the "Bundle is a Fragment" flag set does not contain a BPSec extension block |

## 3.9 RFC 9173 Security Contexts (Parent: REQ-2) - *Optional for initial development*

| ID | Description |
| :--- | :--- |
| **2.2.1** | The bundle parsing functionality may validate BPSec integrity extension blocks using the BIB-HMAC-SHA2 context with a 256-bit hash value |
| **2.2.2** | The bundle parsing functionality may validate BPSec integrity extension blocks using the BIB-HMAC-SHA2 context with a 384-bit hash value |
| **2.2.3** | The bundle parsing functionality may validate BPSec integrity extension blocks using the BIB-HMAC-SHA2 context with a 512-bit hash value |
| **2.2.4** | The bundle parsing functionality may validate BPSec integrity extension blocks using a key-wrap function on HMAC key |
| **2.2.5** | The bundle parsing functionality may validate BPSec confidentiality extension blocks using the BCB-AES-GCM context with a 128-bit symmetric key value |
| **2.2.6** | The bundle parsing functionality may validate BPSec confidentiality extension blocks using the BCB-AES-GCM context with a 256-bit symmetric key value |
| **2.2.7** | The bundle parsing functionality may validate BPSec confidentiality extension blocks using a key-wrap function on AES key |

## 3.10 TCPCLv4 (Parent: REQ-3)

| ID | Description |
| :--- | :--- |
| **3.1.1** | The implementation shall support 'Active' session establishment, as per section 3 of RFC 9174 |
| **3.1.2** | The implementation shall support 'Passive' session establishment, as per section 3 of RFC 9174 |
| **3.1.3** | The implementation shall maintain a pool of idle connections for reuse to avoid the overhead of connection establishment per bundle |
| **3.1.4** | The implementation shall provide its local node ids as part of the session initialization, section 4.6 of RFC 9174 |
| **3.1.5** | The implementation shall allow the administrator to configure the default values for the session parameters, section 4.7 of RFC 9174 |
| **3.1.6** | The implementation shall correctly process the presence of extension items in the session initialization message, section 4.8 of RFC 9174 |
| **3.1.7** | The implementation shall support TLS, section 4.4 of RFC 9174 |
| **3.1.8** | The implementation shall default to using TLS unless explicitly configured not to by an administrator |
| **3.1.9** | The implementation shall support TLS Entity Identification using the DNS Name, and Network Address methods in Section 4.4.1 of RFC 9174 |
| **3.1.10** | The implementation shall support session upkeep messages when negotiated |

## 3.11 EID Patterns (Parent: REQ-6)

| ID | Description |
| :--- | :--- |
| **6.1.1** | The EID pattern parsing functionality shall correctly parse the textual representation of 'ipn' EID patterns |
| **6.1.2** | The EID pattern parsing functionality shall provide a function to determine if a particular EID and EID pattern match |

## 3.12 CLA APIs (Parent: REQ-6)

| ID | Description |
| :--- | :--- |
| **6.1.3** | The implementation shall provide an API to enable CLAs to indicate the success of forwarding |
| **6.1.4** | The implementation shall provide an API for the resolution of EIDs to available CLA addresses, e.g. DNS lookup for TCPCLv4 addresses |

## 3.13 Routing (Parent: REQ-6)

| ID | Description |
| :--- | :--- |
| **6.1.5** | The implementation shall provide the ability for an administrator to specify routing information via a configuration file |
| **6.1.6** | The implementation shall provide an API to allow the addition and removal of routes at runtime |
| **6.1.7** | The implementation shall provide the ability to discard bundles based on the destination |
| **6.1.8** | The implementation shall provide the ability to reflect a bundle back to the previous node on a per-bundle basis |
| **6.1.9** | The implementation shall provide a mechanism to prioritise routing rules to avoid misconfiguration |
| **6.1.10** | The implementation shall implement Equal Cost Multi-Path (ECMP) when multiple CLAs of the same priority can forward a bundle |

## 3.14 Local Disk Storage (Parent: REQ-7)

| ID | Description |
| :--- | :--- |
| **7.1.1** | The implementation shall provide a configurable location for the storage of bundles |
| **7.1.2** | The implementation shall provide a configurable maximum total for all bundle data stored on the local filesystem |
| **7.1.3** | The implementation shall provide a configurable mechanism to control how bundles are discarded when the storage reaches its capacity |

## 3.15 SQLite Storage (Parent: REQ-7)

| ID | Description |
| :--- | :--- |
| **7.2.1** | The implementation shall provide the ability to store and retrieve metadata from an SQLite relational database stored on the local filesystem |
| **7.2.2** | The implementation shall provide a configurable location for the filesystem location of the metadata database |

## 3.16 S3 Storage (Parent: REQ-9)

| ID | Description |
| :--- | :--- |
| **9.1.1** | The implementation shall enable an administrator to configure the location and access credentials for a particular S3 storage instance |
| **9.1.2** | The implementation shall provide a configurable maximum total for all bundle data stored on S3 |
| **9.1.3** | The implementation shall provide a configurable mechanism to control how bundles are discarded when the storage reaches its capacity |
| **9.1.4** | The implementation shall use the common S3 APIs as implemented by the majority of cloud service providers, avoiding implementor specifics wherever possible |

## 3.17 OpenTelemetry (Parent: REQ-19)

| ID | Description |
| :--- | :--- |
| **19.1.1** | All components shall be able to emit OpenTelemetry Log Records via the OpenTelemetry API |
| **19.1.2** | All components shall be able to emit OpenTelemetry Traces via the OpenTelemetry API |
| **19.1.3** | All components shall be able to emit OpenTelemetry Metrics via the OpenTelemetry API |

## 3.18 Tools (Parent: REQ-19)

| ID | Description |
| :--- | :--- |
| **19.2.1** | A tool shall be provided that can send a rate-controlled series of bundles of a user-supplied size to a particular remote endpoint |
| **19.2.2** | A tool shall be provided that can report the successful reception of a response to a bundle reception sent by a remote bundle processing service |
| **19.2.3** | A tool shall be provided that can report the round-trip communication time of a bundle to a remote endpoint |
| **19.2.4** | Tools shall avoid relying on the availability of BPv7 status reports for correct functionality |
| **19.2.5** | Tools shall support being run without requiring a fully functional BPA be installed locally |

## 3.19 Documentation (Parent: REQ-21)

| ID | Description |
| :--- | :--- |
| **21.2.1** | The source, documentation and examples shall be available as a public repository on GitHub.com |
| **21.2.2** | The Rust crates shall be available on crates.io |
| **21.2.3** | The source code documentation shall be made available as RustDoc documentation on docs.rs |

## 3.20 Issue Reporting and Tracking (Parent: REQ-21)

| ID | Description |
| :--- | :--- |
| **21.3.1** | The project shall provide a public Issue Tracker on GitHub.com |
| **21.3.2** | The project shall provide a managed mechanism for reporting security issues through the use of GitHub's private vulnerability reporting feature |

---

# Part 4: Requirements Verification Matrix

This matrix breaks down the top-level technical requirements above to a series of mid-level requirements, and maps each to the verification method. Most of these mid-level requirements are further broken down to an extensive set of individual requirements and test cases, for example when compliance with a particular protocol standard is required. These low-level requirements are generated as part of the development cycle, and their verification is validated via this verification matrix, providing traceability to the top-level requirements and objectives via a level of indirection.

A complete test verification matrix is maintained, mapping every automated test to the one or more requirements that it verifies, ensuring that validation can be performed not only by per-subsystem requirements, but also across the solution as a whole.

| Requirement ID | Description | Verification method |
| :--- | :--- | :--- |
| **1** | **Full compliance with RFC9171** | |
| 1.1 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality RFC9171 | Visual inspection |
| 1.2 | The solution shall demonstrate support via a test verification report of 1.1 | Pass/Fail (from report) |
| **2** | **Full compliance with RFC9172 and RFC9173** | |
| 2.1 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality RFC9172 | Visual inspection |
| 2.2 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality RFC9173 | Visual inspection |
| 2.3 | The solution shall demonstrate support via a test verification report of 2.1 | Pass/Fail (from report) |
| 2.4 | The solution shall demonstrate support via a test verification report of 2.2 | Pass/Fail (from report) |
| **3** | **Full compliance with RFC9174** | |
| 3.1 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality of RFC9174 | Visual inspection |
| 3.2 | The solution shall demonstrate support via a test verification report of 3.1 | Pass/Fail |
| **4** | **Alignment with on-going DTN standardisation** | |
| 4.1 | The solution shall deliver a profile of IETF UDP-CL capability | Visual inspection |
| 4.2 | The solution shall deliver a profile of CCSDS Custody Transfer capability | Visual inspection |
| 4.3 | The solution shall deliver a profile of CCSDS QoS capability | Visual inspection |
| 4.4 | The solution shall deliver a profile of CCSDS Compressed Status Reporting capability | Visual inspection |
| 4.5 | The solution shall deliver a profile of IETF Bundle-in-Bundle Encapsulation capability | Visual inspection |
| 4.6 | The solution shall demonstrate support of the profile via a test verification report of 4.1 | Pass/Fail (from report) |
| 4.7 | The solution shall demonstrate support of the profile via a test verification report of 4.2 | Pass/Fail (from report) |
| 4.8 | The solution shall demonstrate support of the profile via a test verification report of 4.3 | Pass/Fail (from report) |
| 4.9 | The solution shall demonstrate support of the profile via a test verification report of 4.4 | Pass/Fail (from report) |
| 4.10 | The solution shall demonstrate support of the profile via a test verification report of 4.5 | Pass/Fail (from report) |
| **5** | **Experimental support for QUIC** | |
| 5.1 | The solution shall deliver an experimental specification for a protocol for transferring bundles between peers using the QUIC protocol | Visual inspection |
| 5.2 | The solution shall support the experimental specification of 5.1 | Visual inspection |
| 5.3 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality of 5.1 | Visual inspection |
| 5.4 | The solution shall demonstrate support of the protocol via a test verification report of 5.3 | Pass/Fail (from report) |
| 5.5 | The solution shall allow the user to configure QUIC support at runtime | Pass/Fail |
| **6** | **Time-variant Routing API** | |
| 6.1 | The solution shall allow an API client to specify the start of the contact period with a peer | Pass/Fail |
| 6.2 | The solution shall allow an API client to specify the duration of the contact period with a peer | Pass/Fail |
| 6.3 | The solution shall allow an API client to specify the expected transmission data rate achievable with a peer during a contact period | Pass/Fail |
| 6.4 | The solution shall allow an API client to specify the optional periodicity of a contact | Pass/Fail |
| 6.5 | The solution shall allow an API client to specify that a contact period with a peer is expected, ensuring bundles are stored | Pass/Fail |
| 6.6 | The solution shall allow an API client to update contact specification without requiring a system restart | Pass/Fail |
| **7** | **Support for local filesystem for bundle and metadata storage** | |
| 7.1 | The solution shall support the storage of bundles on a local filesystem | Pass/Fail |
| 7.2 | The solution shall support storing additional metadata required for the correct functioning of the system as a whole on a local filesystem | Pass/Fail |
| 7.3 | The solution shall support restarting the system and recovering state from the data stored on the local filesystem | Pass/Fail |
| **8** | **Support for PostgreSQL for bundle metadata storage** | |
| 8.1 | The solution shall support storing additional metadata required for the correct functioning of the system as a whole in a remote PostgreSQL database instance | Pass/Fail |
| 8.2 | The solution shall support restarting the system and recovering state from the data stored on a remote PostgreSQL instance | Pass/Fail |
| **9** | **Support for Amazon S3 storage for bundle storage** | |
| 9.1 | The solution shall support the storage of bundles on a remote system that supports the Amazon S3 API | Pass/Fail |
| 9.2 | The solution shall support restarting the system and recovering state from the data stored on a remote system that supports the Amazon S3 API | Pass/Fail |
| **10** | **Support for Amazon DynamoDB for bundle metadata storage** | |
| 10.1 | The solution shall support storing additional metadata required for the correct functioning of the system as a whole in a remote DynamoDB instance | Pass/Fail |
| 10.2 | The solution shall support restarting the system and recovering state from the data stored on a remote DynamoDB instance | Pass/Fail |
| **11** | **Support for Azure Blob Storage for bundle storage** | |
| 11.1 | The solution shall support the storage of bundles on a remote Azure Blob Storage instance | Pass/Fail |
| 11.2 | The solution shall support restarting the system and recovering state from the data stored on a remote Azure Blob Storage instance | Pass/Fail |
| **12** | **Support for Azure SQL for bundle metadata storage** | |
| 12.1 | The solution shall support storing additional metadata required for the correct functioning of the system as a whole in a remote Azure SQL instance | Pass/Fail |
| 12.2 | The solution shall support restarting the system and recovering state from the data stored on a remote Azure SQL instance | Pass/Fail |
| **13** | **Performance** | |
| 13.1 | The BPA component shall be capable of processing 1000 bundles per second | Pass/Fail |
| 13.2 | The BPA component shall support the fragmentation and reassembly of bundles with a total reassembled size of at least 4GB | Pass/Fail |
| 13.3 | The BPA component shall be capable of the storage of at least 1TB of bundles at rest, for 1 month | Pass/Fail |
| 13.4 | The TCP-CL CLA shall be capable of supporting 10Gbit/s data rates, with 10MB bundle size, 16kB segment size, and 1350 byte link-layer MTU | Pass/Fail |
| 13.5 | The TCP-CL CLA shall be capable of supporting 10Gbit/s data rates, with 1kB bundle size, 1kB segment size, and 576 byte link-layer MTU | Pass/Fail |
| **14** | **Reliability** | |
| 14.1 | The solution will deliver a 'fuzz test' verification matrix mapping every external interface to a test implementation and execution status | Visual inspection |
| **15** | **Independent component packaging** | |
| 15.1 | The solution shall provide all independently deployable components as OCI compliant packaged container images | Visual inspection |
| 15.2 | The solution shall verify the correct installation, update and removal of delivered containers on representative infrastructure | Pass/Fail |
| 15.3 | The solution shall deliver installation documentation for each packaged component | Visual inspection |
| **16** | **Kubernetes packaging** | |
| 16.1 | The solution shall deliver a Helm chart enabling the installation and customisation of the the solution as a whole onto a Kubernetes cluster | Pass/Fail |
| 16.2 | The solution shall deliver installation documentation for the packaged solution | Visual inspection |
| **17** | **Comprehensive usage documentation** | |
| 17.1 | The solution shall deliver documentation describing the purpose, capabilities, and applicability of the solution | Visual inspection |
| 17.2 | The solution shall deliver documentation describing how to complete common installation, configuration, and management tasks of the system as a whole, acting as a quick-start for users | Visual inspection |
| 17.3 | The solution shall deliver detailed configuration documentation, detailing every option and possible values. Validation will be performed via a verification matrix mapping every option to the relevant section in the documentation | Pass/Fail |
| **18** | **Comprehensive technical documentation and examples** | |
| 18.1 | The solution shall deliver sufficient high-level design documentation to assist third-parties in extending the platform for their own uses | Visual inspection |
| 18.2 | The solution shall deliver comprehensive API documentation and sample code, covering all external interfaces | Visual inspection |
| 18.3 | The solution shall deliver a report cross-referencing failure modes of the system and the various types of supported storage, detailing the possibility of data loss | Visual inspection |
| 18.4 | The solution will deliver a reliability report, estimating the Mean Time Between Failure (MTBF) of each delivered component, and aggregate values for the system as a whole | Visual inspection |
| **19** | **A well-featured suite of management and monitoring tools** | |
| 19.1 | The solution shall support the ability to export system logs and traces to external monitoring and traceability tools via the OpenTelemetry APIs | Visual inspection |
| 19.2 | The solution shall deliver tools capable of enabling users to test the correct functioning of the solution as part of a larger DTN network, by injecting and inspecting bundles, and monitoring their path across the network | Visual inspection |
| **20** | **Interoperability with existing implementations** | |
| 20.1 | The solution shall deliver an interoperability verification matrix mapping functionality of the solution, including CLAs and implemented standards, to the following alternate implementations: ION (NASA/JPL), HDTN (NASA/Glenn), DTNME (NASA/Marshal), cFS (NASA/Goddard), μD3TN (D3TN GmbH), ESA BP implementation | Visual inspection |
| 20.2 | The solution shall deliver a compliance verification matrix of interoperable capability to alternative implementation of 20.1 | Pass/Fail |
| **21** | **Available to all under a permissive licence** | |
| 21.1 | The solution shall deliver all source code and binaries under a licence compatible with an ESA Open Source License | Visual inspection |
| 21.2 | All documentation and examples shall be made freely available to the public in an online digital form | Visual inspection |
| 21.3 | The solution shall deliver a suitable mechanism for users to raise potential defects and have visibility of the actions being taken to address such reports | Visual inspection |

---

# Part 5: Initial Development Scope

The initial development focuses on a reasonably achievable subset of requirements that support the overall objectives, reducing delivery risk while establishing a solid foundation. The three objectives described above remain unaltered.

## 5.1 Initial Development Scope

The following high-level requirements are in scope for initial development:

| Requirement | Description |
| :--- | :--- |
| **REQ-1** | Full compliance with RFC9171 |
| **REQ-3** | Full compliance with RFC9174 |
| **REQ-6** | Time-variant Routing API |
| **REQ-7** | Support for local filesystem for bundle and metadata storage |
| **REQ-9** | Support for Amazon S3 storage for bundle storage |
| **REQ-15** | Independent component packaging |
| **REQ-17** | Comprehensive usage documentation |
| **REQ-18** | Comprehensive technical documentation and examples |
| **REQ-19** | A well-featured suite of management and monitoring tools |
| **REQ-21** | Available to all under a permissive licence |

This subset of requirements is considered a "depth over breadth" approach, whereby full functionality over a subset of options is considered more useful than supporting a wide range of options in an incomplete manner. The intention is that the initial development will result in the delivery of a Minimum Viable Product (MVP), providing an opportunity to validate the solution early and ensure it fits with long-term goals.

## 5.2 Optional Additions

If the delivery gets significantly ahead of schedule, the following requirements may be considered for inclusion in initial development:

- **REQ-16**: Kubernetes packaging
- **REQ-20**: Interoperability testing with existing implementations, particularly ION

## 5.3 Initial Development Verification Matrix

The following mid-level system requirements verification matrix applies to initial development:

| Requirement ID | Description | Verification method |
| :--- | :--- | :--- |
| **1** | **Full compliance with RFC9171** | |
| 1.1 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality RFC9171 | Visual inspection |
| 1.2 | The solution shall demonstrate support via a test verification report of 1.1 | Pass/Fail (from report) |
| **3** | **Full compliance with RFC9174** | |
| 3.1 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality of RFC9174 | Visual inspection |
| 3.2 | The solution shall demonstrate support via a test verification report of 3.1 | Pass/Fail (from report) |
| **6** | **Time-variant Routing API** | |
| 6.1 | The solution shall allow an API client to specify the start of the contact period with a peer | Pass/Fail |
| 6.2 | The solution shall allow an API client to specify the duration of the contact period with a peer | Pass/Fail |
| 6.3 | The solution shall allow an API client to specify the expected transmission data rate achievable with a peer during a contact period | Pass/Fail |
| 6.4 | The solution shall allow an API client to specify the optional periodicity of a contact | Pass/Fail |
| 6.5 | The solution shall allow an API client to specify that a contact period with a peer is expected, ensuring bundles are stored | Pass/Fail |
| 6.6 | The solution shall allow an API client to update contact specification without requiring a system restart | Pass/Fail |
| **7** | **Support for local filesystem for bundle and metadata storage** | |
| 7.1 | The solution shall support the storage of bundles on a local filesystem | Pass/Fail |
| 7.2 | The solution shall support storing additional metadata required for the correct functioning of the system as a whole on a local filesystem | Pass/Fail |
| 7.3 | The solution shall support restarting the system and recovering state from the data stored on the local filesystem | Pass/Fail |
| **9** | **Support for Amazon S3 storage for bundle storage** | |
| 9.1 | The solution shall support the storage of bundles on a remote system that supports the Amazon S3 API | Pass/Fail |
| 9.2 | The solution shall support restarting the system and recovering state from the data stored on a remote system that supports the Amazon S3 API | Pass/Fail |
| **15** | **Independent component packaging** | |
| 15.1 | The solution shall provide all independently deployable components as OCI compliant packaged container images | Visual inspection |
| 15.2 | The solution shall verify the correct installation, update and removal of delivered containers on representative infrastructure | Pass/Fail |
| 15.3 | The solution shall deliver installation documentation for each packaged component | Visual inspection |
| **17** | **Comprehensive usage documentation** (\*) | |
| 17.1 | The solution shall deliver documentation describing the purpose, capabilities, and applicability of the solution | Visual inspection |
| 17.2 | The solution shall deliver documentation describing how to complete common installation, configuration, and management tasks of the system as a whole, acting as a quick-start for users | Visual inspection |
| 17.3 | The solution shall deliver detailed configuration documentation, detailing every option and possible values. Validation will be performed via a verification matrix mapping every option to the relevant section in the documentation | Pass/Fail |
| **18** | **Comprehensive technical documentation and examples** (\*) | |
| 18.1 | The solution shall deliver sufficient high-level design documentation to assist third-parties in extending the platform for their own uses | Visual inspection |
| 18.2 | The solution shall deliver comprehensive API documentation and sample code, covering all external interfaces | Visual inspection |
| 18.3 | The solution shall deliver a report cross-referencing failure modes of the system and the various types of supported storage, detailing the possibility of data loss | Visual inspection |
| **19** | **A well-featured suite of management and monitoring tools** (\*) | |
| 19.1 | The solution shall support the ability to export system logs and traces to external monitoring and traceability tools via the OpenTelemetry APIs | Visual inspection |
| 19.2 | The solution shall deliver tools capable of enabling users to test the correct functioning of the solution as part of a larger DTN network, by injecting and inspecting bundles, and monitoring their path across the network | Visual inspection |
| **21** | **Available to all under a permissive licence** | |
| 21.1 | The solution shall deliver all source code and binaries under a licence compatible with an ESA Open Source License | Visual inspection |
| 21.2 | All documentation and examples shall be made freely available to the public in an online digital form | Visual inspection |
| 21.3 | The solution shall deliver a suitable mechanism for users to raise potential defects and have visibility of the actions being taken to address such reports | Visual inspection |

(\*) The requirements for documentation, tooling, and examples shall be reduced to only cover the deliverables of initial development.
