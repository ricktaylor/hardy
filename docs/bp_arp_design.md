# Bundle Protocol Address Resolution Protocol (BP-ARP)

**Status:** Draft Design Document
**Version:** 0.3
**Date:** 2026-01-31

## Abstract

This document specifies Bundle Address Resolution Discovery (BP-ARP), a mechanism for discovering the Bundle Protocol Agent (BPA) Endpoint Identifier (EID) of a node that is reachable at a known Convergence Layer (CL) address but whose EID is unknown.

**Note:** This design must comply with RFC 9758 (Updates to the 'ipn' URI Scheme), which defines LocalNode addressing and imposes constraints on its usage.

## Table of Contents

1. [Introduction](#1-introduction)
2. [Terminology](#2-terminology)
3. [Protocol Overview](#3-protocol-overview)
4. [Normative Specification](#4-normative-specification)
5. [Security Considerations](#5-security-considerations)
6. [IANA Considerations](#6-iana-considerations)
7. [Open Questions](#7-open-questions)
8. [References](#8-references)

---

## 1. Introduction

### 1.1 Problem Statement

In Bundle Protocol networks, a Convergence Layer Adapter (CLA) may discover link-layer adjacency with another node without learning that node's BPA Endpoint Identifier. For example:

- A UDP-based CLA receives packets from a previously unknown IP address
- A Bluetooth CLA discovers a nearby device
- A satellite link becomes available to a ground station

In these cases, the CLA knows *how* to reach the neighbour (CL address) but not *who* they are (EID). Without the EID, the BPA cannot:

- Install routes to the discovered node
- Make forwarding decisions for bundles destined to that node
- Participate in neighbour discovery protocols like SAND

### 1.2 Relationship to SAND

The IETF SAND protocol (draft-ietf-dtn-bp-sand) provides secure advertisement and neighbourhood discovery at the Bundle Protocol layer. However, SAND requires the ability to send bundles to discovered neighbours. BP-ARP provides the bootstrap mechanism to resolve CL addresses to EIDs, enabling SAND to operate.

```
CLA discovers CL adjacency (Neighbour)
    → BP-ARP resolves Neighbour → Peer (learns EID)
        → SAND can exchange topology information with known Peer
```

### 1.3 Design Rationale

BP-ARP is designed as a BPA-level operation, not a service-level one. The question "what is your EID?" is directed at the BPA itself, similar to how ICMP messages in IP are handled by the IP stack rather than applications.

For this reason, BP-ARP uses the administrative endpoint rather than a dedicated service endpoint.

---

## 2. Terminology

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "NOT RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be interpreted as described in BCP 14 [RFC2119] [RFC8174].

**Bundle Protocol Agent (BPA)**
: The node-level entity that implements the Bundle Protocol.

**Convergence Layer Adapter (CLA)**
: A component that adapts the Bundle Protocol to a specific underlying transport.

**Convergence Layer Address (CL Address)**
: A transport-specific address used by a CLA (e.g., IP address + port for TCP/UDP).

**Endpoint Identifier (EID)**
: A name for a Bundle Protocol endpoint, using either the `ipn` or `dtn` URI scheme.

**Peer**
: A remote BPA with a known EID and reachable via a known CL address.

**Neighbour**
: A CL adjacency where the CL address is known but the EID is unknown.

**Address Resolution**
: The process of discovering the EID associated with a Neighbour's CL address.

---

## 3. Protocol Overview

### 3.1 Message Flow

```
    Node A                                      Node B
    (knows B's CL address,                      (EID: ipn:42.0)
     doesn't know B's EID)
         |                                          |
         |  ARP Request                             |
         |  src: ipn:17.0                           |
         |  dst: ipn:!.0                            |
         |  [sent to B's CL address]                |
         |----------------------------------------->|
         |                                          |
         |                            ARP Response  |
         |                            src: ipn:42.0 |
         |                            dst: ipn:17.0 |
         |<-----------------------------------------|
         |                                          |
    Node A learns:                                  |
    B's EID is ipn:42.0                             |
```

### 3.2 Addressing

**RFC 9758 Constraint:**

RFC 9758 Section 3.4.2 defines LocalNode as Node Number 2^32-1 (0xFFFFFFFF), represented as `ipn:!.<service>` or `ipn:4294967295.<service>`.

However, RFC 9758 Section 5.4 states:
> "all externally received bundles featuring LocalNode EIDs as a bundle source or bundle destination MUST be discarded as invalid."

This means LocalNode (`ipn:!.0`) cannot be used as the destination for ARP probes under current RFC 9758 rules.

**RFC 9758 Update:**

This specification updates RFC 9758 to allow externally received bundles with LocalNode as destination for specific administrative record types:

- **Destination `ipn:!.0`**: ARP Request bundles destined to LocalNode MUST be accepted and processed (not discarded)

**Response Source EID:**

ARP Response bundles MUST use the administrative endpoint EID of the responding node as the source. This ensures the response can be delivered to the requester and correctly identifies the BPA. For nodes with:
- IPN EIDs: Use `ipn:<node>.0` (administrative endpoint, service 0)
- DTN EIDs only: Use their DTN node ID (e.g., `dtn://node/`)

See Section 4.1 for the full specification.

### 3.3 Administrative Record Type

BP-ARP request and response messages are encoded as Bundle Protocol Administrative Records, allowing them to be handled by the existing administrative endpoint infrastructure.

---

## 4. Normative Specification

### 4.1 ARP Request Addressing

Due to RFC 9758 Section 5.4 prohibiting externally received LocalNode EIDs, BP-ARP requires an alternative addressing mechanism.

**Solution: RFC 9758 Update**

This specification proposes an update to RFC 9758 that relaxes the LocalNode restriction for specific administrative record types. This update:

- Does not break existing semantics (LocalNode remains non-routable for general use)
- Creates a controlled exception for discovery protocols like BP-ARP
- Allows `ipn:!.0` to be used as the destination for ARP probes

Proposed text for RFC 9758 update:

> Bundles destined to the LocalNode administrative endpoint (`ipn:!.0`) received from external sources MAY be accepted if:
> 1. The bundle is marked as an administrative record, AND
> 2. The administrative record type is registered in an IANA registry for LocalNode acceptance

This approach:
- Limits the exception to the administrative endpoint only (`ipn:!.0`)
- Does not permit external bundles to arbitrary LocalNode services
- Positions ARP correctly as a BPA-level operation
- Maintains security by limiting the exception to registered admin record types
- Allows future protocols to use the same mechanism if needed

**IPN-Only Probing:**

BP-ARP uses IPN addressing exclusively for the probe mechanism. There is no DTN-scheme equivalent (e.g., no `dtn:none/admin` or similar). This is intentional:

- IPN LocalNode (`ipn:!.0`) provides the "control plane" for ARP
- The response payload contains EIDs of all schemes (IPN and DTN)
- IPN-based ARP bootstraps discovery of DTN-scheme EIDs
- Simpler design with a single, well-defined addressing mechanism

### 4.2 ARP Request Format

An ARP Request is a bundle with the following properties:

**Primary Block:**
- Source EID: The requesting node's administrative endpoint (e.g., `ipn:<node>.0`)
- Destination EID: `ipn:!.0` (LocalNode administrative endpoint)
- Bundle Processing Control Flags: Administrative record flag MUST be set

**Hop Count Extension Block:**
- ARP Request bundles SHOULD include a Hop Count extension block (block type 10, per RFC 9171 Section 4.3.3)
- Hop limit SHOULD be set to 1
- This prevents the ARP Request from being routed beyond the immediate neighbour

**Payload Block:**
- Contains an Administrative Record with type TBD (ARP Request)
- Payload content: Empty, or CBOR array of already-known EIDs (for "what else?" queries)

**Transmission:**
- The bundle MUST be sent via the CLA to the specific CL address of the Neighbour
- The bundle MUST NOT be routed through normal RIB lookup
- The Hop Count limit of 1 provides defence-in-depth against mis-routing

### 4.3 ARP Response

Upon receiving an ARP Request, a BPA MUST respond with an ARP Response:

**Primary Block:**
- Source EID: The administrative endpoint EID of the responding node
  - IPN: `ipn:<node>.0` (service 0)
  - DTN: `dtn://node/` (node ID)
- Destination EID: The source EID from the ARP Request

Note: The source MUST be the administrative endpoint EID of the responding node, NOT `ipn:!.0`.

**Payload Block:**
- Contains an Administrative Record with type TBD (ARP Response)
- Payload: CBOR array of all node EIDs (all schemes)

**Payload Format (CDDL):**
```cddl
arp-response = [+ eid]
eid = $eid  ; As defined in RFC 9171 Section 4.2.5.1
```

**Example Response Payload:**
```cbor
[
  [2, [1, 42, 0]],           ; ipn:1.42.0 (3-element encoding)
  [1, "//node42/"]           ; dtn://node42/
]
```

**Processing:**
- The requesting node receives the full list of node EIDs
- All EIDs in the response are associated with the Neighbour's CL address
- The Neighbour is promoted to Peer with all discovered EIDs
- Routes may be installed for each discovered EID

**Benefits:**
- Supports multi-homed nodes with multiple EIDs
- Supports nodes with both IPN and DTN scheme EIDs
- Allows "what else do you have?" queries by including known EIDs in request
- DTN-scheme-only nodes can respond with their EIDs despite IPN-based probing

### 4.4 LocalNode Address Handling

**RFC 9758 Update:**

This specification updates RFC 9758 Section 5.4 to allow externally received bundles destined to the LocalNode administrative endpoint (`ipn:!.0`) for specific administrative record types. This relaxation:

- Applies only to the administrative endpoint (service 0), not arbitrary LocalNode services
- Does not break existing semantics - LocalNode remains non-routable for general use

**Updated Behavior:**

A BPA implementing BP-ARP:
- MUST accept bundles destined to `ipn:!.0` from external sources IF:
  - The bundle is marked as an administrative record, AND
  - The administrative record type is ARP Request (type TBD)
- MUST validate that such bundles are properly formatted
- MUST process valid ARP Requests and generate ARP Responses
- SHOULD apply rate limiting to prevent denial-of-service attacks
- MUST continue to discard other externally received LocalNode destination EIDs unless specifically permitted by a registered administrative record type

**Response Source EID:**

ARP Responses MUST use the administrative endpoint EID of the responding node as the source. This MUST NOT be `ipn:!.0`. DTN-only nodes use their DTN node ID (e.g., `dtn://node/`).

### 4.5 ARP Policy Configuration

The decision to perform ARP resolution is a BPA configuration option, not a CLA implementation choice:

| Policy | Behavior |
|--------|----------|
| `as-needed` | Only probe if CLA provides no EIDs (default) |
| `always` | Always probe, verify/augment CLA-provided EIDs |
| `never` | Trust CLA, fail if no EIDs provided |

This separation ensures:
- CLAs report facts (what they learned from the CL layer)
- Administrators configure policy (trust model for deployment)
- ARP subsystem executes the configured policy

### 4.6 CLA Interface

CLAs report discovered adjacencies using:

```
add_peer(cl_address: ClAddress, eids: &[Eid])
```

Where:
- `eids` is empty: CLA discovered CL adjacency but doesn't know EID (Neighbour)
- `eids` is non-empty: CLA learned one or more EIDs (may be incomplete due to CL limitations)

Multi-homing is supported: a single CL address may be associated with multiple EIDs.

---

## 5. Security Considerations

### 5.1 BPSec Authentication

ARP bundles SHOULD be authenticated using BPSec Block Integrity Block (BIB) as defined in RFC 9172.

**ARP Request:**
- SHOULD include a BIB targeting the payload block
- Security source: The requesting node's administrative endpoint
- Provides proof that the request came from the claimed source

**ARP Response:**
- SHOULD include a BIB targeting the payload block
- Security source: The responding node's administrative endpoint
- Provides proof that the response came from the node claiming that EID

**Assumptions:**

BP-ARP assumes that key material is pre-placed and identity is already established:

- Nodes have pre-configured keys or certificates for BPSec operations
- Trust anchors are provisioned through out-of-band mechanisms
- Credential exchange and identity verification is the domain of SAND

**Scope:**

ARP performs address resolution only - mapping a CL address to EIDs. It does not:

- Establish new trust relationships
- Exchange credentials or certificates
- Perform identity bootstrapping

The BIB authenticates that the ARP response originated from the node claiming the EID. Deployments requiring credential exchange or identity verification SHOULD use SAND after ARP resolution.

### 5.2 Relaxation of LocalNode Rule

Accepting external bundles destined to `ipn:!.0` introduces potential security risks:

- **Spoofing:** A malicious node could send false ARP Responses
- **Denial of Service:** Flooding with ARP Requests could overwhelm a BPA
- **Information Disclosure:** ARP Responses reveal node EIDs

Mitigations:
- BPSec BIB authentication (Section 5.1)
- Rate limiting on ARP Request processing
- Validation that ARP Requests are properly formatted administrative records
- Policy configuration to disable ARP in sensitive environments (`arp = "never"`)

### 5.3 Trust Model

The ARP policy configuration allows administrators to choose appropriate trust levels:

- **Closed networks:** `arp = "never"` - trust CLA-provided EIDs only
- **Open networks:** `arp = "as-needed"` - verify when CLA doesn't provide EID
- **High security:** `arp = "always"` - always verify, even CLA-provided EIDs

### 5.4 Relationship to SAND

BP-ARP and SAND serve complementary roles:

| Protocol | Function |
|----------|----------|
| BP-ARP | Address resolution: maps CL address to EID(s) |
| SAND | Identity verification and credential exchange |

The typical sequence is:

1. CLA discovers link-layer adjacency (Neighbour)
2. BP-ARP resolves Neighbour to Peer (learns EID)
3. SAND exchanges credentials and verifies identity
4. Topology information can be securely exchanged

BP-ARP uses pre-placed keys for BPSec. SAND handles the credential exchange problem for nodes that require dynamic identity bootstrapping.

---

## 6. IANA Considerations

### 6.1 Administrative Record Type

This document requests allocation of a new Bundle Protocol Administrative Record type:

| Value | Description |
|-------|-------------|
| TBD1 | ARP Request |
| TBD2 | ARP Response |

### 6.2 No Service Number Required

BP-ARP uses the existing administrative endpoint (`ipn:X.0`) and does not require allocation of a new service number.

---

## 7. Open Questions

### 7.1 Error Handling

What should happen when a Neighbour doesn't respond?

Considerations:
- Retry count and interval
- Exponential backoff
- Maximum resolution time before giving up
- Whether to report resolution failure to CLA

### 7.2 Pre-filter vs Administrative Record

Should ARP be handled by:
- A pre-filter in the BPA that intercepts before normal admin processing
- A new administrative record type processed by the admin endpoint

The pre-filter approach may be simpler for implementation but less standards-compliant.

### 7.3 SAND Compatibility

Should the ARP response format align with SAND Credential Advertisement for future compatibility? Both convey node identity information, and alignment could simplify implementations that support both protocols.

---

## 8. References

### 8.1 Normative References

- [RFC 9171] Burleigh, S., Fall, K., and E. Birrane, "Bundle Protocol Version 7", RFC 9171, January 2022.
- [RFC 9172] Birrane, E., and K. McKeever, "Bundle Protocol Security (BPSec)", RFC 9172, January 2022.
- [RFC 9758] Taylor, R., and E. Birrane, "Updates to the 'ipn' URI Scheme", RFC 9758, May 2025.
- [RFC 2119] Bradner, S., "Key words for use in RFCs to Indicate Requirement Levels", BCP 14, RFC 2119, March 1997.
- [RFC 8174] Leiba, B., "Ambiguity of Uppercase vs Lowercase in RFC 2119 Key Words", BCP 14, RFC 8174, May 2017.

### 8.2 Informative References

- [draft-ietf-dtn-bp-sand] TBD, "Bundle Protocol Secure Advertisement and Neighbourhood Discovery (SAND)", Internet-Draft.
- [RFC 826] Plummer, D., "An Ethernet Address Resolution Protocol", RFC 826, November 1982.

---

## Appendix A. Implementation Notes

### A.1 Integration with BPA

The BP-ARP subsystem integrates with the BPA as follows:

1. CLA reports `add_peer(cl_address, [])` (empty EID list)
2. BPA checks ARP policy configuration
3. If policy requires probing, ARP subsystem sends Request to `ipn:!.0` via CLA
4. ARP subsystem receives Response, extracts EID from source field
5. Neighbour promoted to Peer with discovered EID
6. RIB updated with route to new Peer

### A.2 Generic Implementation

BP-ARP is implemented generically in the BPA core, not per-CLA:

- Discovery probe is a BP-layer bundle (CL-agnostic)
- CLA provides "send to CL address" capability
- ARP subsystem orchestrates the resolution process
- No duplicate resolution logic needed in each CLA

---

*This document is a work in progress and subject to change.*
