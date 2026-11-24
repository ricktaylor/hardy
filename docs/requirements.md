# Hardy Requirements Specification

## Document Information

| **Field** | **Value** |
| :--- | :--- |
| **Project** | Hardy DTN Framework |
| **Version** | 1.3 |

## Document History

| Version | Baseline | Summary |
| :--- | :--- | :--- |
| 1.2 | Release v0.2.0 | Initial development baseline. |
| 1.3 | | Aligns the requirements with the implemented design and the planned features recorded in the design documents. Withdraws REQ-10, REQ-11 and REQ-12, replaces the MTBF deliverable with soak testing, makes the Cloud the primary rather than the only target environment, and adds Objective 4 and REQ-22 for re-usable libraries on embedded platforms. Recasts Hardy as a framework for building DTN applications, with the Bundle Protocol Agent server as one application built from it, and adds REQ-23 for the framework APIs. Removes the initial development scope (Part 5). Annex A lists every changed requirement. |

## Scope

Hardy is a framework for building Delay-Tolerant Networking applications on the Bundle Protocol. It is a set of independently usable Rust libraries (the CBOR codec, the BPv7 bundle format and BPSec, EID patterns, a Bundle Protocol Agent, convergence layer adaptors, storage backends and routing agents) together with reference applications assembled from them: a Bundle Protocol Agent server, a standalone TCPCLv4 server, a Time-Variant Routing agent, and command-line tools. An integrator can deploy the reference applications as they are, or combine the libraries, with components of their own, into a different DTN node, gateway, tool or application.

The reference applications focus on the terrestrial elements of a heterogeneous Bundle Protocol DTN. The interface between the core functionality and specific communication systems at ground stations is provided via Convergence Layer adaptors designed for Internet usage: initially TCPCLv4 (REQ-3), with experimental support for convergence layer protocols that are not yet standardised (REQ-5), such as QUIC and the unidirectional, frame-based BTP-U.

The core protocol libraries are also usable on embedded and constrained platforms (REQ-22), so the same bundle processing code can run in a ground station and on the equipment at the far end of the link.

Interoperability testing with existing Bundle Protocol implementations with radio-layer specific convergence layer adaptors, such as LTP over AOS, or CCSDS Bundle-in-EPP, ensures that support for specific link-layer technology can be integrated without modification of either implementation.

---

# Part 1: Technical Objectives

The technical objectives of Hardy are to deliver a reliable, performant, secure, interoperable, extensible framework for building Bundle Protocol applications; with the cloud as the primary target environment and re-usable protocol libraries that also serve embedded and constrained platforms, accompanied by high-quality documentation, examples, and tools, made available to all under a permissive licence.

In this document, "the solution" means the framework as a whole: its libraries and its reference applications. A requirement that names a specific crate or application applies to that component only.

## Objective 1: A reliable, performant, secure, interoperable, extensible Bundle Protocol framework

The purpose of the solution is to provide the parts from which nodes within a larger delay-tolerant network are built, and a reference Bundle Protocol Agent server built from them.

As a component within such a network, the critical capabilities of a node, whether the reference server or one an integrator builds, are to:

1. Successfully connect to the network.
2. Remain a member of the network without aborting or misbehaving.
3. Move data across the network, without loss, in a secure and timely manner.

The framework must make those capabilities the default for what is built with it, and the reference server must have them, or the solution cannot be considered fit for purpose.

As the reference applications focus on the ground segment of a heterogeneous DTN, rather than a bespoke space-to-ground DTN solution, the ability to interoperate with other system-of-systems components is vital. Therefore open and de-facto standards are implemented to ensure the widest possible interoperability. The standard selected is the Bundle Protocol Version 7, published by the IETF, and documented, extended, and profiled by the CCSDS. Supporting this standard is a key requirement of compliance with the LunaNet Interoperability Specification.

In order to meet the reliability, performance and security objective, the choice of programming language is key. The solution is implemented in Rust (<https://www.rust-lang.org/>), a compiled, memory-safe, systems-programming language renowned for its reliability, performance, and security. Flight software is almost always required to be implemented in C or ADA, however as there is no flight qualification requirement on the solution a more modern alternative was selected, reducing the time to deliver without compromising performance or correctness. There is increased interest in the automotive industry in Rust for safety critical software, supporting this decision on its suitability.

Where possible, every component is implemented in 'async' Rust (<https://rust-lang.github.io/async-book/>), a state-of-the-art technology maximising the use of multiprocessor platforms by automatically spreading packet processing load across all available cores. Cloud service providers offer compute platforms with a very large number of cores, and ensuring full utilisation of these resources maximises performance.

## Objective 2: The Cloud as the primary target environment

Given one of the key differentiators for the solution is its focus on the Cloud as its primary target environment, it must take advantage of the capabilities of that environment to maximum effect. There is a difference between software that can run in the Cloud, and software that is truly 'Cloud native': the former can survive, the latter can flourish.

One of the major advantages of deploying applications to the Cloud is the ability to scale instances of services, providing increased reliability through redundancy, and increased performance through load balancing. To leverage this capability, the Bundle Protocol Agent is a library embedded in a server application, and convergence layer adaptors, application services and routing agents attach to it either in the same process or as independently deployable containerized components over gRPC, interconnected via the Cloud-provider high-speed network.

Although Cloud service providers support virtualized disks for applications to use for storage, they also provide managed storage services at considerably better capacity, reliability, and performance price-points, but only if applications support their APIs. Given a Bundle Protocol Agent needs to store both the payload and the metadata describing the payload, the solution is modular with respect to its use of storage: a "plug-in" model provides the common bundle processing capability with the ability to use native Cloud data storage services for both metadata and payload, configurable at time of deployment.

The solution targets common technology available across the majority of cloud service providers, without tuning for any one of them. Bundle data uses the S3 object storage API, which most providers implement, and bundle metadata uses PostgreSQL, which every major provider offers as a managed service. Provider-specific storage services are not targeted; the storage plug-in model allows a third party to add one without modifying the Bundle Protocol Agent.

The Cloud is the primary target environment, not the only one: Objective 4 extends the protocol libraries to other applications and to embedded and constrained platforms.

## Objective 3: High-quality documentation, examples, and tools, available to all

Beyond the list of functional capabilities, there are operational aspects that are just as important to the success of the solution: No matter how well-featured, if the solution is almost impossible to install, configure, manage, and monitor, either in isolation or as part of a larger system, then it is essentially useless. For a framework, the same holds for building with it: if a developer cannot learn how to compose the libraries and extend them, they will not be used.

To that end, the solution is supported by well-written documentation for two audiences: operators, who need the general capabilities and usage of the reference applications and how to configure and integrate them into a larger system; and developers, who need to know how to build their own DTN applications from the libraries and how to extend them with components of their own.

To provide operators with the ability to monitor and manage any deployment, the solution supports modern Cloud-native observability tools, and provides a suite of diagnostic tools to aid the deployment and monitoring of the solution.

## Objective 4: Re-usable protocol libraries, including for embedded and constrained platforms

Each library of the framework is useful on its own. A tool that inspects bundles needs the BPv7 library but not a Bundle Protocol Agent; a gateway needs a Bundle Protocol Agent and a convergence layer but not the reference server; a link-layer engine is useful to an implementation that is not Hardy at all. Each library therefore depends only on what it needs, has a public API that a third party can build against, and is published separately.

The most demanding re-use is on constrained platforms. A DTN is only as interoperable as its least capable node, and the far end of a ground-station link is often a constrained platform: a 32-bit microcontroller with no operating system and little memory. Running the same protocol implementation at both ends of a link removes a whole class of interoperability defects.

The core protocol libraries (CBOR, the BPv7 bundle format and BPSec, EID patterns, link-layer protocol engines, and the Bundle Protocol Agent) therefore build without the Rust standard library, using only a heap allocator, and behave correctly on 32-bit targets. Platform services such as an async runtime, a clock or a cryptographic random number generator are supplied by the integrator rather than assumed.

This objective does not make the solution flight software. No flight qualification is claimed, and the reference server applications remain cloud and ground-segment deployments.

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
- Bundle-in-Bundle Encapsulation, including segmented encapsulation as the successor to RFC 9171 fragmentation
- BPSec COSE security contexts
- EID patterns
- Bundle Protocol Managed Information (CCSDS 734.20-O-1 Annex C)

The solution is designed to be extensible in order to support development of these and other future extensions, and act as a suitable testbed for the evaluation at scale of such extensions. Extensions that are not yet standardised integrate in the same way as the built-in components, so a third party can implement one, including in closed source, without modifying the Bundle Protocol Agent.

This requirement is a goal of the solution, as the final outcome cannot be clearly specified. A support matrix maps the capabilities of pre-standard extensions to the capabilities of the solution implementation, indicating either support, or noting the reasons for any absence. Verification of capability is performed by automated unit or component testing. Use of extensions is configurable.

*This requirement supports Objective 1.*

## REQ-5: Experimental and pre-standard convergence layer protocols

TCPCLv4 (REQ-3) is the standardised baseline for exchanging bundles between nodes, but it assumes a reliable, bidirectional IP path. Convergence layer protocols for other links, and for newer transports, are still being researched and standardised in the IETF and CCSDS. To assist the progress of that experimentation and standardisation, and to act as a testbed for it, the solution shall include experimental support for convergence layer protocols that are not yet standardised, following the IETF draft specification of each where one exists.

The protocols in scope are:

- **QUIC.** There is active research in the deep space community concerning the applicability of recent Internet transport protocols to deep-space use cases. The IETF Proposed Standard "RFC 9000 QUIC: A UDP-Based Multiplexed and Secure Transport" may deliver performance and scalability advantages over TCP-CL for the terrestrial segment of a DTN.
- **Bundle Transfer Protocol - Unidirectional (BTP-U).** Many space and radio links offer no IP services and no return channel: broadcast radio, satellite downlinks, and CCSDS frame-based links. BTP-U carries bundles over such links in fixed or variable size frames, without acknowledgements.

Other experimental convergence layer protocols may be added to this list as they emerge.

Each protocol is implemented as a convergence layer adaptor in the same way as TCPCLv4 (REQ-23), so it can be deployed with the reference applications or with an integrator's own, and its protocol engine is usable independently of the Bundle Protocol Agent where the protocol allows.

This requirement is a goal of the solution, as the final outcome cannot be clearly specified. For each protocol, a support matrix maps the capabilities of the protocol specification to the capabilities of the convergence layer implementation, indicating either support, or noting the reasons for any absence. Verification of capability is performed by automated unit or component testing. Use of each protocol is configurable.

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

The available bandwidth of a contact shall be applied to the egress scheduling of the bundles forwarded during that contact, rather than only recorded.

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

## REQ-10: Support for Amazon DynamoDB for bundle metadata storage (withdrawn)

Withdrawn in version 1.3. Provider-specific storage services are no longer targeted (Objective 2): PostgreSQL (REQ-8) provides managed metadata storage on every major cloud provider. The identifier is reserved and not reused.

## REQ-11: Support for Azure Blob Storage for bundle storage (withdrawn)

Withdrawn in version 1.3. Provider-specific storage services are no longer targeted (Objective 2): the S3 API (REQ-9) provides object storage across cloud providers. The identifier is reserved and not reused.

## REQ-12: Support for Azure SQL for bundle metadata storage (withdrawn)

Withdrawn in version 1.3. Provider-specific storage services are no longer targeted (Objective 2): PostgreSQL (REQ-8) provides managed metadata storage on every major cloud provider. The identifier is reserved and not reused.

## REQ-13: Performance

Given the target of the solution is the ground segment, no differentiation between 'uplink' or 'downlink' is made. Peak performance of the solution is dependent on the underlying capabilities of the network and network interfaces.

The BPA component shall be capable of supporting, at minimum:

- The processing of 1000 bundles per second.
- The reassembly of fragmented bundles with a total reassembled size of at least 4GB.
- The reception, storage and forwarding of a single bundle of at least 4GB, without holding the whole bundle in memory.
- The storage of at least 1TB of bundles at rest, for 1 month.

The TCP-CL convergence layer adaptor shall be capable of supporting, at minimum:

- 10Gbit/s data rates, with the following combinations of bundle, CL segment size, and link-layer maximum transmittable unit (MTU) sizes:
  - 10MB bundle size, 16kB segment size, 1350 byte link-layer MTU
  - 1kB bundle size, 1kB segment size, 576 byte link-layer MTU

The solution targets 100% compliance. This is confirmed through automated testing. This requirement is validated via a verification matrix mapping required performance to the achieved result, broken down by convergence-layer and target environment configuration.

*Note: Additional performance can be achieved by increasing the size of the underlying virtual machine, and/or scaling out the number of instances of the BPA and TCP-CL, but the requirements above set the baseline for a single instance with a single network interface.*

*This requirement supports Objective 1.*

## REQ-14: Reliability

The reliability of the solution is demonstrated during development and testing, with a goal of maximising reliability. Reliability is evidenced by three verifiable deliverables rather than a calculated Mean Time Between Failure figure, because a defensible MTBF needs field failure data that a software-only deliverable cannot produce:

- **Soak testing.** Each deployable component is run under sustained, representative load, possibly derived from the data used for performance validation, and the results are published: duration, throughput, memory and file-handle growth, and any failures.
- **Failure-mode analysis.** A report cross-references the failure modes of the system (application abort, host power loss, storage failure) against each supported storage backend, stating the possibility of data loss and the recovery behaviour.
- **Robustness testing.** Every internal and external API is checked for correctness with unit, component, or 'fuzz' testing, to ensure that incorrect or malicious input does not cause any component to abort or become compromised.

*This requirement supports Objective 1.*

## REQ-15: Independent component packaging

In order to simplify the installation and management of the reference applications of the solution, each is packaged as a container image in a manner compliant with the Open Container Initiative standards on packaging (<https://opencontainers.org/>), with complete supporting documentation.

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

Just like correctly deploying and configuring an IP router, successfully deploying a BPv7 bundle agent can be non-trivial. Each independently configurable reference application has a potentially large number of options that may have a major impact on the capability of a deployment as a whole. Correctly configuring them is critical to maximising performance or security for a particular deployment.

Therefore the reference applications are accompanied by complete usage documentation for operators. The documentation includes a general overview of the capabilities and usage of the solution, as well as accurate technical information concerning the configuration options available.

In order to keep pace with the rapid rate of development, documentation is published in a publicly accessible digital form, and kept up to date with each release.

The solution targets 100% compliance. This is confirmed by visual inspection. This requirement is validated via a verification matrix mapping each configurable option of every component to a corresponding section of the documentation.

*This requirement supports Objective 3.*

## REQ-18: Comprehensive technical documentation and examples

Although the reference applications are usable stand-alone, the solution is designed to be part of a larger system-of-systems, and to be built upon. It exposes APIs allowing third parties to extend the reference applications, to build applications of their own from its libraries (REQ-23), and to integrate with existing ground-segment capabilities.

In order to make extending a running Bundle Protocol Agent as painless as possible from any language, its external process APIs use the de-facto gRPC standard (<https://grpc.io/>). The gRPC APIs carry the same authority as an in-process component, so they are protected: a client must authenticate before it may register.

Accompanying every API, gRPC and Rust alike, is complete documentation of its correct usage and documented example source code. Design documentation describes the architecture of each library and how the libraries compose.

The solution targets 100% compliance. This is confirmed by visual inspection. This requirement is validated via a verification matrix mapping each API function of every external interface and each extension point to a corresponding section of the documentation and example.

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
- dtn7-rs
- Unibo-BP (University of Bologna)

This requirement is a goal of the solution, as access to functional versions of the above implementations may not always be possible. A compatibility matrix is published mapping the BPv7 capabilities of each implementation above to the interoperable capability of the solution, including the required configuration of both implementations and the target environment. This is confirmed by automated testing. This requirement is validated via a verification matrix mapping each BPv7 function to a corresponding interoperability statement and test result.

*This requirement supports Objective 1.*

## REQ-21: Available to all under a permissive licence

In order to maximise the availability of the solution to all, the solution is licensed and made available under a permissive open-source licence.

This is verified by inspection.

*This requirement supports Objective 3.*

## REQ-22: Embedded and constrained platform support

The core protocol library crates shall build for bare-metal 32-bit targets without the Rust standard library, using only a heap allocator, and shall behave correctly where pointers are 32 bits wide. The crates in scope are the CBOR codec, the BPv7 bundle and BPSec library, the EID pattern library, the BTP-U protocol engine, and the Bundle Protocol Agent library.

Platform services (an async runtime, a clock, a cryptographic random number generator, atomic operations a target lacks) are supplied by the integrator rather than assumed.

The solution targets 100% compliance. This is confirmed by continuous integration builds of every crate in scope for a 32-bit bare-metal target, including one without atomic compare-and-swap for the crates that do not need it. This requirement is validated via a verification matrix mapping each crate to its target build and test status.

*This requirement supports Objectives 1 and 4.*

## REQ-23: Composable framework APIs

The solution is a framework: its libraries are the product, and the reference applications are examples of what can be built from them. An integrator shall be able to build a DTN application, including a closed-source one, from the published library crates without modifying them.

An integrator can supply their own implementation of every extension point of the Bundle Protocol Agent (convergence layer adaptors, application services, routing agents, storage backends, egress scheduling policies, bundle filters, and BPSec key material), and can do anything the reference applications do using the same published APIs. Each library is independently usable, and an application includes only the optional capabilities it uses.

The public APIs of the library crates follow Semantic Versioning, so an integrator can depend on a released version and know which upgrades are compatible. Each crate states its minimum supported Rust version, and changes are recorded in a per-crate change log.

The solution targets 100% compliance. This is confirmed by automated build and test of the reference applications against the public APIs, and by visual inspection of the documentation and examples. This requirement is validated via a verification matrix mapping each extension point to its documentation and a working example.

*This requirement supports Objectives 1, 3 and 4.*

---

# Part 3: Low-Level Requirements (LLR)

This document introduces additional Low-level Requirements, derived from the High- and Mid-level requirements specified in Part 2.

Not all high- or mid-level requirements have derived requirements, only those where additional specificity is required.

All requirements detailed here are validated through automated tests, and verified by examination of test reports.

An LLR identifier is never reused. A withdrawn LLR keeps its row, marked as withdrawn with its reason, and a reworded LLR keeps its identifier when its intent is unchanged. LLRs added in version 1.3 continue the numbering of the section they belong to, or open a new section numbered after the Part 4 requirement they derive from.

The implementation and verification status of each requirement is recorded in the requirements coverage report, not in this document.

## Standards Compliance (1.1)

| ID | Description |
| :--- | :--- |
| **1.1.1** | The implementation shall be compliant with all mandatory requirements of the CCSDS Bundle Protocol Specification (CCSDS 734.20-O-1), excluding the ADU Fragmentation process as defined in Section 5.8 of RFC 9171 |

## CBOR Encoding (1.1)

| ID | Description |
| :--- | :--- |
| **1.1.2** | The CBOR encoding functionality must support the explicit emission of tagged and untagged data types, to ensure deterministically encoded CBOR, section 4.2 of RFC 8949, can be parsed appropriately |
| **1.1.3** | The CBOR encoding functionality must support all the major types as defined in section 3.1 of RFC 8949 |
| **1.1.4** | The CBOR encoding functionality must emit all primitive types in canonical form, to ensure deterministically encoded CBOR, section 4.2 of RFC 8949, can be used appropriately |
| **1.1.5** | The CBOR encoding functionality must ensure that all Maps and Arrays have the correct number of data items for definite length sequences |

## CBOR Decoding (1.1)

| ID | Description |
| :--- | :--- |
| **1.1.7** | The CBOR decoding functionality must report if a parsed data item is in canonical form |
| **1.1.8** | The CBOR decoding functionality must report if a parsed data item has any associated tags |
| **1.1.9** | The CBOR decoding functionality must support all the primitive data items defined in the CBOR specification |
| **1.1.10** | The CBOR decoding functionality must ensure that data items contained within the serialization of a CBOR Map or Array are parsed within the context of the relevant sequence correctly |
| **1.1.11** | The CBOR decoding functionality must support the ability to opportunistically parse an item from a byte sequence, and only return a data item if one exists |
| **1.1.12** | The CBOR decoding functionality must indicate if an incomplete data item is found at the end of a byte sequence, which could be correctly parsed if more bytes were to be provided |

## CBOR General (1.1)

| ID | Description |
| :--- | :--- |
| **1.1.13** | The CBOR processing functionality should be suitable for use in embedded platforms (`no_std`) |

## BPv7 Parsing (1.1)

| ID | Description |
| :--- | :--- |
| **1.1.14** | *Withdrawn in 1.3.* The bundle parsing functionality does not rewrite bundles: non-canonical input is rejected (LLR 1.1.31) |
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

## BPv7 Bundle Generation (1.1)

| ID | Description |
| :--- | :--- |
| **1.1.25** | The bundle creation and rewriting functionality must generate valid, canonical CBOR encoded bundles |
| **1.1.26** | The bundle creation and rewriting functionality must only include valid, canonical CBOR encoded extension blocks as part of a bundle |
| **1.1.27** | The bundle creation and rewriting functionality must apply the required CRC values to all generated bundles |
| **1.1.28** | The bundle creation and rewriting functionality must apply the required CRC values to all generated extension blocks |
| **1.1.29** | The bundle creation functionality must allow a caller to specify the CRC type to be applied to the new bundle |

## BPv7 Bundle Processing (1.1)

| ID | Description |
| :--- | :--- |
| **1.1.30** | The bundle processing functionality must enforce the bundle rewriting rules when discarding unrecognised extension blocks, including removing any BPSec targets that reference them |
| **1.1.31** | The bundle processing functionality must reject a received or service-originated bundle whose CBOR encoding is not deterministic as required by section 4.1 of RFC 9171, and must not rewrite it into canonical form |
| **1.1.32** | The bundle parsing functionality must report the reason each extension block is to be removed (unrecognised with "delete block on failure" set, or a security operation failure under section 5.1.1 of RFC 9172) such that a security policy enforcing function can determine the correct action to take |
| **1.1.33** | The bundle processing functionality must recognise and process the Bundle Age extension block when determining if a bundle's lifetime has expired |
| **1.1.34** | The bundle processing functionality must process and act on the Hop Count extension block |

## Bundle Pipeline (1.2)

| ID | Description |
| :--- | :--- |
| **1.2.1** | A received bundle that is rejected (invalid, expired, hop limit exceeded, duplicate, dropped by a filter, or routed to an explicit drop) shall not be stored, and the CLA shall be told so it can stop the transfer |
| **1.2.2** | A received bundle that ends before it is complete, or that exceeds the configured maximum bundle size, shall be refused without a status report, so the sending CLA does not acknowledge it |
| **1.2.3** | When a bundle is rejected after its primary block has been validated, the Bundle Protocol Agent shall generate the reception and deletion status reports the bundle requests, as one status report carrying both assertions, as described in sections 5.6 and 5.10 of RFC 9171 |
| **1.2.4** | The Bundle Protocol Agent shall generate forwarding and delivery status reports when the bundle requests them, as described in sections 5.4 and 5.7 of RFC 9171 |
| **1.2.5** | A bundle whose lifetime expires while in storage shall be deleted, with a deletion status report citing lifetime expiry when the bundle requests one |
| **1.2.6** | Bundles originated by local services shall pass the same validation and filtering as received bundles |
| **1.2.7** | The Bundle Protocol Agent shall reassemble fragmented bundles destined for a local service before delivery, as described in section 5.9 of RFC 9171 |
| **1.2.8** | The memory the Bundle Protocol Agent uses for a bundle on reception, forwarding and delivery shall be bounded independently of the bundle's payload size |
| **1.2.9** | ADU reassembly shall be bounded by the configured maximum bundle size; overlapping fragments shall be accepted, keeping the first bytes received; and a fragment carrying a payload integrity block shall be accepted, with the integrity of the reassembled payload verified |
| **1.2.10** | Status reports for locally originated bundles dropped by an Originate filter, and for bundles dropped by a Deliver filter, shall follow the same reception and deletion report rules as ingress (LLR 1.2.3) |

## Bundle Filters (1.3)

| ID | Description |
| :--- | :--- |
| **1.3.1** | The Bundle Protocol Agent shall provide filter hooks at Ingress, Originate, Egress and Deliver, to which an integrator can add filters of their own |
| **1.3.2** | A Verifier filter shall be able to drop a bundle, with a status report reason, at the Ingress, Originate and Deliver hooks |
| **1.3.3** | A Classifier filter shall be able to drop a bundle, or annotate it with metadata that the routing decision can read, at the Ingress and Originate hooks; the annotations persist with the bundle |
| **1.3.4** | A Rewriter filter shall be able to add, replace and remove extension blocks at the Egress and Deliver hooks |
| **1.3.5** | A filter shall be able to read the bundle's blocks and a bounded prefix of its payload |
| **1.3.6** | When the input filter configuration changes, stored bundles shall be classified again before they are next routed; a bundle a Verifier then drops is deleted with a deletion report, not a new reception report |
| **1.3.7** | The Bundle Protocol Agent shall allow selected bundles to be inspected by an external component, which releases each one, optionally with a new classification, or drops it with a reason |

## BPSec (2.1)

| ID | Description |
| :--- | :--- |
| **2.1.1** | The bundle parsing functionality shall validate BPSec integrity and confidentiality blocks for validity according to the abstract block syntax as specified in RFC 9172 |
| **2.1.2** | The bundle parsing functionality shall correctly remove BPSec target information from any affected confidentiality or integrity block, when the targeted block is removed |
| **2.1.3** | The bundle signing and encryption functionality shall not add a BIB or BCB to a bundle with the "Bundle is a fragment" flag set, as required by section 5.2 of RFC 9172 |
| **2.1.4** | The bundle signing functionality shall add BPSec integrity blocks over a caller-selected set of target blocks, acting as a BPSec security source |
| **2.1.5** | The bundle encryption functionality shall add BPSec confidentiality blocks over a caller-selected set of target blocks, acting as a BPSec security source |
| **2.1.6** | BPSec key material shall be supplied by the integrator, and may differ per bundle and per security source |
| **2.1.7** | A forwarding node that holds no key for a BPSec operation shall forward the target and its security block unchanged, except where it cannot read a Bundle Age block it needs to determine expiry, in which case the bundle shall be rejected |
| **2.1.8** | A node that holds the key for a BPSec integrity operation shall verify it on reception; a target that fails verification shall be handled as required by section 5.1.1 of RFC 9172 (remove the target and its security operations, or discard the bundle if the target is the payload or primary block) |
| **2.1.9** | A decrypted payload shall not be exposed to any bundle filter |
| **2.1.10** | Key material shall not appear in logs, error messages or diagnostic output, and shall not remain in memory after use |
| **2.1.11** | The Bundle Protocol Agent shall add BIBs and BCBs, according to a configured security policy, to the bundles it originates and forwards, acting as security source; and shall act as a security acceptor at a waypoint when its policy names it one |
| **2.1.12** | A bundle at its destination that cannot yet be decrypted for want of a key shall be delivered once the key becomes available, rather than dropped |
| **2.1.13** | The memory used to encrypt or decrypt a payload shall be bounded independently of the payload's size, and decrypted output shall not be delivered as complete until its authenticity has been verified |
| **2.1.14** | Status reports for bundles discarded by security processing shall carry the BPSec reason codes defined in section 11.2 of RFC 9172 rather than a generic reason |
| **2.1.15** | The security policy shall be configurable for the handling of security operation failures and for requiring protection on specified blocks, as described in section 5.1 of RFC 9172 |
| **2.1.16** | The implementation shall support the BPSec COSE security contexts once their IETF specification is stable |

## RFC 9173 Security Contexts (2.2)

| ID | Description |
| :--- | :--- |
| **2.2.1** | The bundle processing functionality shall create and validate BPSec integrity extension blocks using the BIB-HMAC-SHA2 context with a 256-bit hash value |
| **2.2.2** | The bundle processing functionality shall create and validate BPSec integrity extension blocks using the BIB-HMAC-SHA2 context with a 384-bit hash value |
| **2.2.3** | The bundle processing functionality shall create and validate BPSec integrity extension blocks using the BIB-HMAC-SHA2 context with a 512-bit hash value |
| **2.2.4** | The bundle processing functionality shall create and validate BPSec integrity extension blocks using a key-wrap function on HMAC key |
| **2.2.5** | The bundle processing functionality shall create and validate BPSec confidentiality extension blocks using the BCB-AES-GCM context with a 128-bit symmetric key value |
| **2.2.6** | The bundle processing functionality shall create and validate BPSec confidentiality extension blocks using the BCB-AES-GCM context with a 256-bit symmetric key value |
| **2.2.7** | The bundle processing functionality shall create and validate BPSec confidentiality extension blocks using a key-wrap function on AES key |
| **2.2.8** | The BIB-HMAC-SHA2 context shall compute the integrity-protected plaintext of a primary-block target from the bare canonical primary block, as RFC 9173 erratum 9192 corrects; it shall sign only in that form, and for a transition period shall also accept the form published in RFC 9173 Appendix A.3, logging each such acceptance |

## TCPCLv4 (3.1)

| ID | Description |
| :--- | :--- |
| **3.1.1** | The implementation shall support 'Active' session establishment, as per section 3 of RFC 9174 |
| **3.1.2** | The implementation shall support 'Passive' session establishment, as per section 3 of RFC 9174 |
| **3.1.3** | *Withdrawn in 1.3.* Connection reuse is a means of meeting the performance requirements (REQ-13), not a requirement in its own right |
| **3.1.4** | The implementation shall provide its local node ids as part of the session initialization, section 4.6 of RFC 9174 |
| **3.1.5** | The implementation shall allow the administrator to configure the default values for the session parameters, section 4.7 of RFC 9174 |
| **3.1.6** | The implementation shall correctly process the presence of extension items in the session initialization message, section 4.8 of RFC 9174 |
| **3.1.7** | The implementation shall support TLS, section 4.4 of RFC 9174 |
| **3.1.8** | The implementation shall default to using TLS unless explicitly configured not to by an administrator |
| **3.1.9** | The implementation shall support TLS Entity Identification using the DNS Name, and Network Address methods in Section 4.4.1 of RFC 9174 |
| **3.1.10** | The implementation shall support session upkeep messages when negotiated |
| **3.1.11** | The implementation shall authenticate a peer's Node ID against the NODE-ID subject alternative name of its TLS certificate, section 4.4.4.3 of RFC 9174 |
| **3.1.12** | A received transfer that the Bundle Protocol Agent refuses because it ended incomplete shall be refused to the peer with a reason that permits retransmission |

## Bundle-in-Bundle Encapsulation (4.5)

| ID | Description |
| :--- | :--- |
| **4.5.1** | The implementation shall encapsulate a bundle inside the payload of another bundle (Bundle-in-Bundle Encapsulation), with tunnel endpoints routable as ordinary peers |
| **4.5.2** | Bundle-in-Bundle Encapsulation shall support segmented encapsulation, carrying one inner bundle across several outer bundles and reassembling it at the tunnel exit |
| **4.5.3** | A transient failure to dispatch a decapsulated inner bundle shall not cause the inner bundle to be lost |

## BTP-U (5.6)

| ID | Description |
| :--- | :--- |
| **5.6.1** | The implementation shall provide a BTP-U protocol engine, usable independently of the Bundle Protocol Agent, that carries bundles over fixed or variable size link frames as specified in the IETF BTP-U draft |
| **5.6.2** | The BTP-U receiver shall continue to operate within its configured resource limits when given malformed, repeated or flooding input |
| **5.6.3** | A BTP-U convergence layer adaptor shall carry bundles over a unidirectional link |

## EID Patterns (6.1)

| ID | Description |
| :--- | :--- |
| **6.1.1** | The EID pattern parsing functionality shall correctly parse the textual representation of 'ipn' EID patterns |
| **6.1.2** | The EID pattern parsing functionality shall provide a function to determine if a particular EID and EID pattern match |
| **6.1.11** | The EID pattern parsing functionality shall parse and match 'dtn' scheme EID patterns as an optional feature, pending their standardisation |

## CLA APIs (6.1)

| ID | Description |
| :--- | :--- |
| **6.1.3** | The implementation shall provide an API to enable CLAs to indicate the success of forwarding |
| **6.1.4** | The implementation shall provide an API for the resolution of EIDs to available CLA addresses, e.g. DNS lookup for TCPCLv4 addresses |
| **6.1.12** | The implementation shall allow a CLA to report each adjacent peer, with its node EIDs, as it appears and disappears |
| **6.1.13** | A forwarding transfer that fails shall return the bundle to routing rather than dropping it |
| **6.1.14** | A CLA shall be told whether the Bundle Protocol Agent accepted or refused each received bundle, so that it can relay the outcome to its peer |
| **6.1.15** | The implementation shall establish adjacencies on demand: configured peers are connected at start and reconnected after a loss, and a bundle routed to a node with no adjacency causes the node to be resolved and connected |
| **6.1.16** | A transfer accepted by a CLA but never resolved shall not leave its bundle stranded: the bundle returns to routing, and expires with a status report if its lifetime ends first |

## Routing (6.1)

| ID | Description |
| :--- | :--- |
| **6.1.5** | The implementation shall provide the ability for an administrator to specify routing information via a configuration file |
| **6.1.6** | The implementation shall provide an API to allow the addition and removal of routes at runtime |
| **6.1.7** | The implementation shall provide the ability to discard bundles based on the destination |
| **6.1.8** | The implementation shall provide the ability to reflect a bundle back to the previous node on a per-bundle basis |
| **6.1.9** | The implementation shall provide a mechanism to prioritise routing rules to avoid misconfiguration |
| **6.1.10** | The implementation shall implement Equal Cost Multi-Path (ECMP) when multiple CLAs of the same priority can forward a bundle |
| **6.1.17** | A bundle with no matching route shall wait, and be routed again when the routing table changes, rather than being dropped; a bundle for one of the node's own service endpoints with no registered service shall wait for that service to register |
| **6.1.18** | Routing agents shall be able to install, replace and withdraw routes at runtime through a routing agent API, both in-process and over gRPC |
| **6.1.19** | Static routes read from a configuration file shall be reloaded when the file changes, without a restart |
| **6.1.20** | A static routes file that fails to parse on reload shall leave the existing static routes in force and log the error |
| **6.1.21** | The implementation shall support multiple routing tables, with the table used for each bundle chosen per bundle, and a lookup that misses in the chosen table shall wait rather than fall through to another table |

## Local Disk Storage (7.1)

| ID | Description |
| :--- | :--- |
| **7.1.1** | The implementation shall provide a configurable location for the storage of bundles |
| **7.1.2** | *Withdrawn in 1.3.* Storage capacity is bounded by the Bundle Protocol Agent for every backend alike, not by each backend (LLR 7.3.6) |
| **7.1.3** | *Withdrawn in 1.3.* Discard at capacity is decided by the Bundle Protocol Agent for every backend alike, not by each backend (LLR 7.3.7) |

## SQLite Storage (7.2)

| ID | Description |
| :--- | :--- |
| **7.2.1** | The implementation shall provide the ability to store and retrieve metadata from an SQLite relational database stored on the local filesystem |
| **7.2.2** | The implementation shall provide a configurable location for the filesystem location of the metadata database |

## Storage and Recovery (7.3)

These requirements apply to the Bundle Protocol Agent and every storage backend alike, and therefore also derive from Part 4 requirements 8.2 and 9.2.

| ID | Description |
| :--- | :--- |
| **7.3.1** | Bundle data storage and bundle metadata storage shall be selectable independently of each other |
| **7.3.2** | After a restart, every stored bundle shall resume processing from where it had reached |
| **7.3.3** | Storage recovery shall reconcile the two stores: bundle data with no metadata is processed again as a received bundle; metadata whose data is missing is deleted with a deletion status report citing depleted storage; duplicate and unparseable data is removed |
| **7.3.4** | Storage recovery shall run when the operator requests it at start-up |
| **7.3.5** | The number of bundles that can wait for forwarding shall be limited by storage capacity, not by memory, and waiting shall neither block the sender nor drop bundles |
| **7.3.6** | The in-memory storage backends shall provide a configurable maximum capacity, evicting the least valuable entries (expired before live) when it is reached |
| **7.3.7** | The Bundle Protocol Agent shall provide a configurable maximum total of stored bundle data for any backend, and a configurable eviction order for when that maximum is reached |
| **7.3.8** | A crash during a write shall never leave a partially written bundle that is treated as complete, for any backend |
| **7.3.9** | Storage recovery shall run automatically after an unclean shutdown; the operator can still force it |
| **7.3.10** | Storage recovery shall report why it ran, its progress, each finding and a summary, and shall offer a check-only mode that repairs nothing and exits with a status reflecting what it found |

## S3 Storage (9.1)

| ID | Description |
| :--- | :--- |
| **9.1.1** | The implementation shall enable an administrator to configure the location and access credentials for a particular S3 storage instance |
| **9.1.2** | *Withdrawn in 1.3.* Storage capacity is bounded by the Bundle Protocol Agent for every backend alike, not by each backend (LLR 7.3.6) |
| **9.1.3** | *Withdrawn in 1.3.* Discard at capacity is decided by the Bundle Protocol Agent for every backend alike, not by each backend (LLR 7.3.7) |
| **9.1.4** | The implementation shall use the common S3 APIs as implemented by the majority of cloud service providers, avoiding implementor specifics wherever possible |

## Egress Scheduling (13.1)

| ID | Description |
| :--- | :--- |
| **13.1.1** | The order and timing in which bundles are transmitted to each peer shall be determined by a selectable scheduling policy |
| **13.1.2** | With no policy configured, bundles shall be transmitted to each peer in first-in, first-out order with no shaping |
| **13.1.3** | A scheduling policy shall never drop a bundle; only expiry, eviction and an explicit drop route discard bundles |
| **13.1.4** | The open-source distribution shall include a fair-queuing, pacing scheduling policy |
| **13.1.5** | Traffic classes shall be definable, with their scheduling and storage properties, and assignable to bundles; with no configuration there is one default class |
| **13.1.6** | The available bandwidth of a TVR contact shall be applied as the transmission rate of the peer it describes for the duration of the contact |

## Process Integration APIs (18.2)

| ID | Description |
| :--- | :--- |
| **18.2.1** | Convergence layer adaptors, application services and routing agents shall be attachable to a running Bundle Protocol Agent from another process over gRPC, with the same behaviour as when attached in-process |
| **18.2.2** | The Application API shall let a service send and receive payloads with the Bundle Protocol Agent building and parsing the bundle; the Service API shall let a service send and receive whole bundles, which the Bundle Protocol Agent still validates |
| **18.2.3** | CLAs, services and routing agents shall be registrable and unregistrable at runtime, without a restart |
| **18.2.4** | A gRPC client shall authenticate before its registration is accepted, and shall be authorised only for the kinds of interface (CLA, service or routing agent) its identity permits; credentials shall be rotatable without interrupting service, and revoking one shall end the sessions it authenticated |
| **18.2.5** | Components shall be able to receive the administrative records of the types they handle that are addressed to the node |

## Embedded Platforms (22.1)

| ID | Description |
| :--- | :--- |
| **22.1.1** | The CBOR, BPv7, EID pattern and BTP-U crates shall build without the Rust standard library for a 32-bit bare-metal target |
| **22.1.2** | The BTP-U crate shall build for a 32-bit bare-metal target without atomic compare-and-swap |
| **22.1.3** | The Bundle Protocol Agent library shall build without the Rust standard library for a 32-bit bare-metal target |
| **22.1.4** | The async primitives crate shall support a `no_std` embedded async runtime as well as Tokio |

## Framework APIs (23.1)

| ID | Description |
| :--- | :--- |
| **23.1.1** | An integrator shall be able to supply their own convergence layer adaptors, application services, routing agents, storage backends, egress scheduling policies, bundle filters and BPSec key material to the Bundle Protocol Agent library |
| **23.1.2** | Anything the reference applications do with the library crates, an integrator shall be able to do using the published library crates |
| **23.1.3** | Each library crate shall be usable without the reference applications, and an application shall be able to include only the optional capabilities it uses |
| **23.1.4** | Each library crate shall be published to crates.io with its public API documented in RustDoc |
| **23.1.5** | Each library crate shall declare a minimum supported Rust version |
| **23.1.6** | Each library crate shall record its changes in a per-crate change log, and its releases shall follow Semantic Versioning |
| **23.1.7** | Each extension point of 23.1.1 shall have a documented, buildable example of an integrator's own implementation |
| **23.1.8** | A developer guide shall describe how to build a DTN application from the library crates, and how to implement each extension point of 23.1.1 |

## OpenTelemetry (19.1)

| ID | Description |
| :--- | :--- |
| **19.1.1** | All components shall be able to emit OpenTelemetry Log Records via the OpenTelemetry API |
| **19.1.2** | All components shall be able to emit OpenTelemetry Traces via the OpenTelemetry API |
| **19.1.3** | All components shall be able to emit OpenTelemetry Metrics via the OpenTelemetry API |

## Tools (19.2)

| ID | Description |
| :--- | :--- |
| **19.2.1** | A tool shall be provided that can send a rate-controlled series of bundles of a user-supplied size to a particular remote endpoint |
| **19.2.2** | A tool shall be provided that can report the successful reception of a response to a bundle reception sent by a remote bundle processing service |
| **19.2.3** | A tool shall be provided that can report the round-trip communication time of a bundle to a remote endpoint |
| **19.2.4** | Tools shall avoid relying on the availability of BPv7 status reports for correct functionality |
| **19.2.5** | Tools shall support being run without requiring a fully functional BPA be installed locally |

## Documentation (21.1, 21.2)

| ID | Description |
| :--- | :--- |
| **21.2.1** | The source, documentation and examples shall be available as a public repository on GitHub.com |
| **21.2.2** | The Rust crates shall be available on crates.io |
| **21.2.3** | The source code documentation shall be made available as RustDoc documentation on docs.rs |

## Issue Reporting and Tracking (21.3)

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
| 4.11 | The solution shall deliver a profile of IETF BPSec COSE security context capability, and demonstrate support of it via a test verification report | Pass/Fail (from report) |
| 4.12 | The solution shall deliver a profile of CCSDS Bundle Protocol Managed Information (CCSDS 734.20-O-1 Annex C) capability | Visual inspection |
| **5** | **Experimental and pre-standard convergence layer protocols** | |
| 5.1 | The solution shall deliver an experimental specification for a protocol for transferring bundles between peers using the QUIC protocol | Visual inspection |
| 5.2 | The solution shall support the experimental specification of 5.1 | Visual inspection |
| 5.3 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality of 5.1 | Visual inspection |
| 5.4 | The solution shall demonstrate support of the protocol via a test verification report of 5.3 | Pass/Fail (from report) |
| 5.5 | The solution shall allow the user to configure QUIC support at runtime | Pass/Fail |
| 5.6 | The solution shall support the IETF draft specification of the Bundle Transfer Protocol - Unidirectional (BTP-U) | Pass/Fail |
| 5.7 | The solution shall deliver a compliance verification matrix of supported capability to mandated/optional functionality of the BTP-U specification | Visual inspection |
| 5.8 | The solution shall demonstrate support of BTP-U via a test verification report of 5.7 | Pass/Fail (from report) |
| 5.9 | The solution shall allow the user to configure BTP-U support at runtime | Pass/Fail |
| **6** | **Time-variant Routing API** | |
| 6.1 | The solution shall allow an API client to specify the start of the contact period with a peer | Pass/Fail |
| 6.2 | The solution shall allow an API client to specify the duration of the contact period with a peer | Pass/Fail |
| 6.3 | The solution shall allow an API client to specify the expected transmission data rate achievable with a peer during a contact period | Pass/Fail |
| 6.4 | The solution shall allow an API client to specify the optional periodicity of a contact | Pass/Fail |
| 6.5 | The solution shall allow an API client to specify that a contact period with a peer is expected, ensuring bundles are stored | Pass/Fail |
| 6.6 | The solution shall allow an API client to update contact specification without requiring a system restart | Pass/Fail |
| 6.7 | The solution shall apply the transmission data rate specified for a contact (6.3) to the forwarding of bundles to that peer during the contact | Pass/Fail |
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
| **10** | **Support for Amazon DynamoDB for bundle metadata storage** (withdrawn) | |
| 10.1–10.2 | Withdrawn with REQ-10 | — |
| **11** | **Support for Azure Blob Storage for bundle storage** (withdrawn) | |
| 11.1–11.2 | Withdrawn with REQ-11 | — |
| **12** | **Support for Azure SQL for bundle metadata storage** (withdrawn) | |
| 12.1–12.2 | Withdrawn with REQ-12 | — |
| **13** | **Performance** | |
| 13.1 | The BPA component shall be capable of processing 1000 bundles per second | Pass/Fail |
| 13.2 | The BPA component shall support the reassembly of fragmented bundles with a total reassembled size of at least 4GB | Pass/Fail |
| 13.6 | The BPA component shall receive, store and forward a single bundle of at least 4GB without holding the whole bundle in memory | Pass/Fail |
| 13.3 | The BPA component shall be capable of the storage of at least 1TB of bundles at rest, for 1 month | Pass/Fail |
| 13.4 | The TCP-CL CLA shall be capable of supporting 10Gbit/s data rates, with 10MB bundle size, 16kB segment size, and 1350 byte link-layer MTU | Pass/Fail |
| 13.5 | The TCP-CL CLA shall be capable of supporting 10Gbit/s data rates, with 1kB bundle size, 1kB segment size, and 576 byte link-layer MTU | Pass/Fail |
| **14** | **Reliability** | |
| 14.1 | The solution will deliver a 'fuzz test' verification matrix mapping every external interface to a test implementation and execution status | Visual inspection |
| 14.2 | The solution shall deliver a soak test report for each deployable component, stating the load, duration, throughput, memory and file-handle growth, and any failures observed | Visual inspection |
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
| 18.1 | The solution shall deliver sufficient high-level design documentation to assist third parties in extending the solution and building their own applications from it | Visual inspection |
| 18.2 | The solution shall deliver comprehensive API documentation and sample code, covering all external interfaces | Visual inspection |
| 18.3 | The solution shall deliver a report cross-referencing failure modes of the system and the various types of supported storage, detailing the possibility of data loss | Visual inspection |
| 18.4 | Withdrawn in version 1.3: replaced by the soak test report (14.2) and the failure-mode report (18.3) | — |
| **19** | **A well-featured suite of management and monitoring tools** | |
| 19.1 | The solution shall support the ability to export system logs and traces to external monitoring and traceability tools via the OpenTelemetry APIs | Visual inspection |
| 19.2 | The solution shall deliver tools capable of enabling users to test the correct functioning of the solution as part of a larger DTN network, by injecting and inspecting bundles, and monitoring their path across the network | Visual inspection |
| **20** | **Interoperability with existing implementations** | |
| 20.1 | The solution shall deliver an interoperability verification matrix mapping functionality of the solution, including CLAs and implemented standards, to the following alternate implementations: ION (NASA/JPL), HDTN (NASA/Glenn), DTNME (NASA/Marshal), cFS (NASA/Goddard), μD3TN (D3TN GmbH), ESA BP implementation, dtn7-rs, Unibo-BP (University of Bologna) | Visual inspection |
| 20.2 | The solution shall deliver a compliance verification matrix of interoperable capability to alternative implementation of 20.1 | Pass/Fail |
| **21** | **Available to all under a permissive licence** | |
| 21.1 | The solution shall deliver all source code and binaries under a licence compatible with an ESA Open Source License | Visual inspection |
| 21.2 | All documentation and examples shall be made freely available to the public in an online digital form | Visual inspection |
| 21.3 | The solution shall deliver a suitable mechanism for users to raise potential defects and have visibility of the actions being taken to address such reports | Visual inspection |
| **22** | **Embedded and constrained platform support** | |
| 22.1 | The solution shall build the core protocol library crates of REQ-22 without the Rust standard library for a 32-bit bare-metal target, in continuous integration | Pass/Fail |
| 22.2 | The solution shall deliver a verification matrix mapping each core protocol library crate to its bare-metal build and test status | Visual inspection |
| **23** | **Composable framework APIs** | |
| 23.1 | The solution shall build its reference applications only from the public APIs of its library crates, in continuous integration | Pass/Fail |
| 23.2 | The solution shall deliver a verification matrix mapping each extension point to its documentation and a working example | Visual inspection |
| 23.3 | The solution shall deliver a developer guide describing how to build DTN applications from the library crates and extend them | Visual inspection |

---

# Annex A: Changes from Version 1.2

This annex lists every requirement that differs from the version 1.2 baseline. Identifiers that are not listed are unchanged.

## Objectives and scope

| Item | Change |
| :--- | :--- |
| Project title | "Hardy DTN Router" becomes "Hardy DTN Framework". |
| Scope | Hardy is a framework of libraries for building DTN applications, with reference applications built from it, rather than a single router. Generalises the experimental convergence layers (REQ-5) and adds embedded platforms (REQ-22). |
| Part 1 introduction | Objectives are for a framework; defines "the solution" as the libraries and reference applications together. Names embedded and constrained platforms alongside the Cloud. |
| Objective 1 | A framework from which nodes are built, plus a reference server, instead of a single node. "No spaceflight requirement" becomes "no flight qualification requirement", consistent with Objective 4. |
| Objective 2 | The Cloud becomes the primary target environment rather than the only one. Describes the Bundle Protocol Agent as a library embedded in a server application, with components attached in-process or over gRPC. Drops the AWS and Azure specialisation in favour of the S3 API and PostgreSQL. |
| Objective 3 | Documentation serves developers building with the framework as well as operators. |
| Objective 4 | New: re-usable protocol libraries, including for embedded and constrained platforms. |

## High-level requirements

| Item | Change |
| :--- | :--- |
| REQ-4 | Adds segmented BIBE, BPSec COSE contexts, EID patterns and BP Managed Information to the tracked extensions, and states that extensions integrate like the built-in components. |
| REQ-5 | Generalised from QUIC to experimental and pre-standard convergence layer protocols; adds BTP-U alongside QUIC, each following its IETF draft where one exists. |
| REQ-6 | Contact bandwidth is applied to egress scheduling, not only recorded. |
| REQ-10, REQ-11, REQ-12 | Withdrawn. |
| REQ-13 | Splits 4GB reassembly from a new single 4GB streamed bundle target. |
| REQ-14 | Replaces the MTBF figure with soak testing, failure-mode analysis and robustness testing. |
| REQ-15 | Packages the reference applications. |
| REQ-17 | Usage documentation is for operators of the reference applications. |
| REQ-18 | Covers building on the solution as well as extending it; adds Rust API documentation and gRPC client authentication. |
| REQ-20 | Adds dtn7-rs and Unibo-BP. |
| REQ-22 | New: embedded and constrained platform support. |
| REQ-23 | New: composable framework APIs. |

## Low-level requirements

| Item | Change |
| :--- | :--- |
| 1.1.14 | Withdrawn: the parser never rewrites bundles. |
| 1.1.30 | Adds the removal of BPSec targets that reference a discarded block. |
| 1.1.31 | Reversed: non-canonical input is rejected, never rewritten. |
| 1.1.32 | Reports the reason each block is to be removed, instead of the reason for a rewrite. |
| 1.2.1–1.2.10 | New section: bundle pipeline. |
| 1.3.1–1.3.7 | New section: bundle filters. |
| 2.1, 2.2 headings | No longer marked optional. |
| 2.1.3 | Restated as a constraint on adding BPSec blocks to a fragment (RFC 9172 section 5.2), not a parser check. |
| 2.1.4–2.1.16 | New: signing, encryption, integrator-supplied keys, keyless forwarding, verification on reception, payload confidentiality from filters, key hygiene, and further BPSec features. |
| 2.2.1–2.2.7 | "May validate" becomes "shall create and validate". |
| 2.2.8 | New: RFC 9173 erratum 9192 primary-block IPPT. |
| 3.1.3 | Withdrawn: connection reuse is a means of meeting REQ-13, not a requirement. |
| 3.1.11, 3.1.12 | New: Node ID authentication, retransmittable refusal. |
| 4.5.1–4.5.3 | New section: Bundle-in-Bundle Encapsulation. |
| 5.6.1–5.6.3 | New section: BTP-U. |
| 6.1.11–6.1.21 | New: 'dtn' EID patterns, CLA peer reporting, transfer outcomes, acceptance reporting, adjacencies on demand, unresolved transfers, default-to-wait, routing agent API, static route reload, multiple routing tables. |
| 7.1.2, 7.1.3, 9.1.2, 9.1.3 | Withdrawn: capacity and discard move to the Bundle Protocol Agent (7.3.6, 7.3.7). |
| 7.3.1–7.3.10 | New section: storage and recovery. |
| 13.1.1–13.1.6 | New section: egress scheduling. |
| 18.2.1–18.2.5 | New section: process integration APIs. |
| 22.1.1–22.1.4 | New section: embedded platforms. |
| 23.1.1–23.1.8 | New section: framework APIs. |

## Verification matrix

| Item | Change |
| :--- | :--- |
| 4.11, 4.12 | New: BPSec COSE and BP Managed Information profiles. |
| 5 heading | Generalised to experimental and pre-standard convergence layer protocols; 5.1–5.5 unchanged (QUIC). |
| 5.6–5.9 | New: BTP-U support, compliance matrix, test report and configuration. |
| 6.7 | New: contact data rate applied to forwarding. |
| 10.x, 11.x, 12.x | Withdrawn with their high-level requirements. |
| 13.2 | Reassembly only; fragmentation by the agent is not required. |
| 13.6 | New: single 4GB streamed bundle. |
| 14.2 | New: soak test report. |
| 18.1 | Design documentation also supports building applications from the solution. |
| 18.4 | Withdrawn: replaced by 14.2 and 18.3. |
| 20.1 | Adds dtn7-rs and Unibo-BP. |
| 22.1, 22.2 | New: embedded platform build and verification matrix. |
| 23.1–23.3 | New: public-API build, extension point matrix, developer guide. |

## Initial development scope

| Item | Change |
| :--- | :--- |
| Part 5 | Removed. The initial development scope, its optional additions and its verification matrix described the first delivery, which is complete; every requirement in this document now applies in full. |
