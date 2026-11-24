# Test Plan: Bundle-in-Bundle Encapsulation

| Document Info | Details |
| :--- | :--- |
| **Functional Area** | Bundle-in-Bundle Encapsulation (BIBE) |
| **Module** | `hardy-bibe` |
| **Implements** | `hardy_bpa::cla::Cla` (encapsulation) and `hardy_bpa::services::Service` (decapsulation) |
| **Standard** | RFC 9171 Section 3.2; [draft-ietf-dtn-bibect](https://datatracker.ietf.org/doc/draft-ietf-dtn-bibect/) |
| **Requirements Ref** | [REQ-4](../../docs/requirements.md#req-4-alignment-with-on-going-dtn-standardisation), [LLR 4.5.x](../../docs/requirements.md#bundle-in-bundle-encapsulation-45) |
| **Test Suite ID** | PLAN-BIBE-01 |
| **Version** | 1.0 |

## 1. Introduction

This document details the testing strategy for the `hardy-bibe` crate, which tunnels bundles through intermediate DTN networks by carrying an inner bundle in the payload of an outer bundle. Encapsulation is performed by a CLA that intercepts forwarded bundles; decapsulation by a service that receives outer bundles, extracts the inner bundle, and dispatches it into the BPA. See [design.md](design.md).

**Scope:**

* **PDU codec:** encoding an inner bundle into a BIBE protocol data unit and decoding it back, including the rejection of malformed and unsupported PDUs.
* **Tunnel behaviour:** encapsulation and decapsulation through the BPA.

## 2. Requirements Mapping

| LLR ID | Description | Verified By |
| :--- | :--- | :--- |
| **4.5.1** | Encapsulate a bundle inside the payload of another bundle, with tunnel endpoints routable as ordinary peers | BIBE-01 (codec); tunnel routing is not yet covered (BIBE-07) |
| **4.5.2** | Segmented encapsulation across several outer bundles | Not implemented; BIBE-02 verifies that segmented PDUs are currently refused |
| **4.5.3** | A transient failure to dispatch a decapsulated inner bundle does not lose it | Not implemented; no test (BIBE-08) |

## 3. Unit Test Cases

### 3.1 PDU Codec

| Test ID | Scenario | Source | Input | Expected Result |
| :--- | :--- | :--- | :--- | :--- |
| **BIBE-01** | **Round trip** | `src/pdu.rs::test_encap_decap_round_trip` | Inner bundle, encapsulated then decapsulated | Inner bundle's destination, source and lifetime survive unchanged |
| **BIBE-02** | **Segmented PDU refused** | `src/pdu.rs::test_decap_rejects_segment` | PDU with a non-zero transmission ID, total length or offset | `Error::SegmentedPdu` |
| **BIBE-03** | **Trailing data refused** | `src/pdu.rs::test_decap_trailing_garbage` | PDU followed by an extra CBOR item | `Error::Cbor(AdditionalItems)` |
| **BIBE-04** | **Wrong segment type refused** | `src/pdu.rs::test_decap_wrong_segment_type` | PDU whose segment is not a byte string | `Error::Cbor(IncorrectType)` |
| **BIBE-05** | **Short PDU refused** | `src/pdu.rs::test_decap_short_array` | PDU array with too few elements | Decapsulation fails |

### 3.2 Tunnel Behaviour (TODO)

| Test ID | Scenario | Source | Procedure | Expected Result |
| :--- | :--- | :--- | :--- | :--- |
| **BIBE-06** | **Segmented round trip** (LLR 4.5.2) | TODO | Encapsulate an inner bundle larger than the segment size, deliver the outer bundles out of order | Inner bundle reassembled intact at the tunnel exit |
| **BIBE-07** | **Tunnel routing** (LLR 4.5.1) | TODO | Register a tunnel endpoint as a peer and install a `via` route through it | A bundle matching the route is encapsulated and sent to the tunnel's remote endpoint |
| **BIBE-08** | **Transient dispatch failure** (LLR 4.5.3) | TODO | Make the inner bundle's dispatch fail transiently during decapsulation | The outer bundle is not deleted, and the inner bundle is delivered once dispatch succeeds |

## 4. Execution Strategy

* **Unit Tests:** `cargo test -p hardy-bibe`
