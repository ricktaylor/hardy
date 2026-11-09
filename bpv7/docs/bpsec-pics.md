# **Protocol Implementation Conformance Statement (PICS)**

## **For RFC 9172: Bundle Protocol Security (BPSec)**

### **Introduction**

This document provides a Protocol Implementation Conformance Statement (PICS) for Bundle Protocol Security (BPSec), as specified in RFC 9172, together with the default security contexts specified in RFC 9173. An implementation claiming conformance to this protocol must satisfy the mandatory requirements specified in this PICS.

### **Implementation Identification**

| Implementation Information | Response |
| :---- | :---- |
| **Supplier** | Aalyria Technologies Inc. |
| **Contact Point for Queries** | Rick Taylor (<rtaylor@aalyria.com>) |
| **Implementation Name(s) and Version(s)** | Hardy, main branch (commit e7694992) |
| **Hardware/Software Environment** | GCP/KVM, Linux 6.1.0-52-cloud-amd64 x86_64 GNU/Linux |
| **Other Information** | |

### **Protocol Summary**

| Protocol Information | Response |
| :---- | :---- |
| **Protocol Title** | Bundle Protocol Security (BPSec) |
| **Standard Reference** | RFC 9172; RFC 9173 (default security contexts) |
| **Date of Standard** | January 2022 |

### **Instructions for Completing the PICS Proforma**

* **Status Column:**
  * **M:** Mandatory
  * **O:** Optional
* **Support Column:**
  * **Y:** Yes, the feature is implemented.
  * **P:** Partially implemented; see the numbered note.
  * **N:** No, the feature is not implemented.
  * **N/A:** Not applicable.

### **PICS Proforma Tables**

#### **Security Block Format and Interaction Requirements**

| Item Number | Item | Protocol Feature | Reference | Status | Support |
| :---: | :---- | :---- | :---- | :---: | :---: |
| 1 | Unique Security Ops | The same security service MUST NOT be applied to a security target more than once in a bundle. | RFC 9172 Section 3.2 | M | Y |
| 2 | Multi-Op Security Blocks | A single security block MAY represent multiple security operations. | RFC 9172 Section 3.3 | O | Y |
| 3 | Target By Block Number | A security target in a security block MUST be represented as the block number of the target block. | RFC 9172 Section 3.4 | M | Y |
| 4 | ASB Structure | The fields of the ASB SHALL be as specified in RFC 9172, Section 3.6. | RFC 9172 Section 3.6 | M | Y |
| 5 | BIB Block Type Code | The block type code value of a BIB SHALL be as specified in Section 11.1 of RFC 9172. | RFC 9172 Section 3.7 | M | Y |
| 6 | BIB ASB Structure | The block-type-specific data field of a BIB SHALL follow the structure of the ASB. | RFC 9172 Section 3.7 | M | Y |
| 7 | BIB Target Restrictions | A security target listed in the Security Targets field of a BIB MUST NOT reference a security block defined in RFC 9172 (e.g., a BIB or a BCB). | RFC 9172 Section 3.7 | M | Y |
| 8 | BIB Authentication | The security context MUST utilize an authentication mechanism or an error detection mechanism. | RFC 9172 Section 3.7 | M | Y |
| 9 | BCB Block Type Code | The block type code value of a BCB SHALL be as specified in Section 11.1 of RFC 9172 | RFC 9172 Section 3.8 | M | Y |
| 10 | BCB Fragment Replication | BCBs MUST have the ‘Block must be replicated in every fragment’ flag set if one of the targets is the payload block. | RFC 9172 Section 3.8 | M | Y |
| 11 | BCB Removal Flag | BCBs MUST NOT have the ‘Block must be removed from bundle if it cannot be processed’ flag set. | RFC 9172 Section 3.8 | M | Y |
| 12 | BCB ASB Structure | The block-type-specific data fields of a BCB SHALL follow the structure of the ASB. | RFC 9172 Section 3.8 | M | Y |
| 13 | BCB Target Types | A security target listed in the Security Targets field of a BCB SHALL be able to reference the payload block, a non-security extension block, or a BIB. | RFC 9172 Section 3.8 | M | Y |
| 14 | BCB-BCB Targeting | A BCB MUST NOT include another BCB as a security target. | RFC 9172 Section 3.8 | M | Y |
| 15 | BCB Primary Block | A BCB MUST NOT target the primary block. | RFC 9172 Section 3.8 | M | Y |
| 16 | BCB-BIB Targeting | A BCB MUST NOT target a BIB unless it shares a security target with that BIB. | RFC 9172 Section 3.8 | M | P (1) |
| 17 | BCB AEAD Cipher | BCBs MUST utilize a confidentiality cipher that provides AEAD. | RFC 9172 Section 3.8 | M | Y |
| 18 | Cipher Suite Info Placement | Additional information created by a cipher suite MAY be placed either in a security result field or in the generated ciphertext. | RFC 9172 Section 3.8 | O | Y |
| 19 | In-Place Encryption | When a BCB is applied, the security target body data SHALL be encrypted ‘in-place’. | RFC 9172 Section 3.8 | M | Y |
| 20 | BIB Full-Overlap Encryption | When adding a BCB to a bundle, if some (or all) of the security targets of the BCB match all of the security targets of an existing BIB, then the existing BIB MUST also be encrypted. | RFC 9172 Section 3.9 | M | Y |
| 21 | BIB Partial-Overlap Split | When adding a BCB to a bundle, if some (or all) of the security targets of the BCB match some (but not all) of the security targets of a BIB, then that BIB MUST be altered in the following way. Any security results in the BIB associated with the BCB security targets MUST be removed from the BIB and placed in a new BIB.  This newly created BIB MUST then be encrypted. | RFC 9172 Section 3.9 | M | N/A (2) |
| 22 | BIB After BCB | A BIB MUST NOT be added for a security target that is already the security target of a BCB | RFC 9172 Section 3.9 | M | Y |
| 23 | Encrypted BIB Not Checked | A BIB integrity value MUST NOT be checked if the BIB is the security target of an existing BCB. | RFC 9172 Section 3.9 | M | Y |
| 24 | Primary Block Canonical Form | The canonical form of the primary block is as specified in reference [2] with the following constraint: CBOR values from the primary block MUST be canonicalized using the rules for Deterministically Encoded CBOR, as specified in [9]. | RFC 9172 Section 4 | M | Y |
| 25 | Non-Primary Canonical Form | All non-primary blocks share the same block structure and are canonicalized as specified in reference [2] with the following constraints.  CBOR values from the non-primary block MUST be canonicalized using the rules for Deterministically Encoded CBOR, as specified in [9].  Only the block-type-specific data field may be provided to a cipher suite for encryption as part of a confidentiality security service.  Fields other than the block-type-specific data within a non-primary block MUST NOT be encrypted or decrypted and MUST NOT be included in the canonical form used by the cipher suite for encryption and decryption | RFC 9172 Section 4 | M | Y |
| 26 | Extended Integrity Scope | An integrity-protection mechanism MAY be applied to fields other than the block-type-specific data within a non-primary block as supported by the security context. | RFC 9172 Section 4 | O | Y |
| 27 | Canonical Reserved Flags | Reserved and unassigned flags in the block processing control flags field MUST be set to 0 in a canonical form. | RFC 9172 Section 4 | M | P (3) |

#### **Security Processing Requirements**

| Item Number | Item | Protocol Feature | Reference | Status | Support |
| :---: | :---- | :---- | :---- | :---: | :---: |
| 28 | BCB Acceptor Determination | If a received bundle contains a BCB, the receiving node MUST determine whether it is the security acceptor for any of the security operations in the BCB. | RFC 9172 Section 5.1.1 | M | Y |
| 29 | BCB Acceptor Processing | If the receiving node is the security acceptor for any of the security operations in the BCB, the node MUST process those operations and remove any operation-specific information from the BCB prior to delivering data to an application at the node or forwarding the bundle. | RFC 9172 Section 5.1.1 | M | Y (4) |
| 30 | BCB Failure Policy | If processing a BCB security operation fails, the target SHALL be processed according to the security policy. | RFC 9172 Section 5.1.1 | M | Y (5) |
| 31 | BCB Failure Report | If processing a BCB security operation fails, a bundle status report indicating the failure MAY be generated. | RFC 9172 Section 5.1.1 | O | Y (6) |
| 32 | Empty BCB Removal | When all security operations for a BCB have been removed from the BCB, the BCB MUST be removed from the bundle. | RFC 9172 Section 5.1.1 | M | Y |
| 33 | Destination Decryption | If the receiving node is the destination of the bundle, the node MUST decrypt any BCBs remaining in the bundle. | RFC 9172 Section 5.1.1 | M | Y (7) |
| 34 | Waypoint BCB Policy | If the receiving node is not the destination of the bundle, the node MUST process the BCB if directed to do so as a matter of security policy. | RFC 9172 Section 5.1.1 | M | Y (8) |
| 35 | Missing BCB Policy | If the security policy of a node specifies that a node should have applied confidentiality to a specific security target and no such BCB is present in the bundle, then the node MUST process this security target in accordance with the security policy. | RFC 9172 Section 5.1.1 | M | Y (9) |
| 36 | Payload Removal Discard | If the payload block is removed as a result of security processing, the bundle MUST be discarded. | RFC 9172 Section 5.1.1 | M | Y |
| 37 | Payload Decrypt Failure | If an encrypted payload block cannot be decrypted (i.e., the ciphertext cannot be authenticated), then the bundle MUST be discarded and processed no further. | RFC 9172 Section 5.1.1 | M | Y |
| 38 | Non-Payload Decrypt Failure | If an encrypted security target other than the payload block cannot be decrypted, then the associated security target and all security blocks associated with that target MUST be discarded and processed no further. | RFC 9172 Section 5.1.1 | M | Y |
| 39 | Security Deletion Reports | As a result of security operations, if a block is deleted from a bundle or if a bundle is dropped, requested status reports (see reference [2]) MAY be generated to reflect bundle or block deletion. | RFC 9172 Section 5.1.1 | O | Y (6) |
| 40 | Plaintext Replacement | When a BCB is decrypted, the recovered plaintext for each security target MUST replace the ciphertext in each of the security targets’; block-type-specific data fields. | RFC 9172 Section 5.1.1 | M | Y |
| 41 | Byte String Reframing | If the plaintext is of a different size than the ciphertext, the framing of the CBOR byte string of this field MUST be updated to ensure this field remains a valid CBOR byte string. | RFC 9172 Section 5.1.1 | M | Y |
| 42 | Multi-Op BCB Processing | If a BCB contains multiple security operations, each operation processed by the node MUST be treated as if the security operation has been represented by a single BCB with a single security operation for the purposes of report generation and policy processing. | RFC 9172 Section 5.1.1 | M | Y |
| 43 | BIB Acceptor Determination | If a received bundle contains a BIB, the receiving node MUST determine whether it is the security acceptor for any of the security operations in the BIB. | RFC 9172 Section 5.1.2 | M | Y (10) |
| 44 | BIB Acceptor Processing | If the receiving node is the security acceptor for any security operations in a BIB, the node MUST process those operations and remove any operation-specific information from the BIB prior to delivering data to an application at the node or forwarding the bundle. | RFC 9172 Section 5.1.2 | M | Y (10) |
| 45 | BIB Failure Policy | If processing a BIB security operation fails, the target SHALL be processed according to the security policy. | RFC 9172 Section 5.1.2 | M | Y (5) |
| 46 | BIB Failure Report | If processing a BIB security operation fails, a bundle status report indicating the failure MAY be generated. | RFC 9172 Section 5.1.2 | O | Y (6) |
| 47 | Empty BIB Removal | When all security operations for a BIB have been removed from the BIB, the BIB MUST be removed from the bundle. | RFC 9172 Section 5.1.2 | M | Y |
| 48 | BIB Under BCB Not Processed | A BIB MUST NOT be processed if the security target of the BIB is also the security target of a BCB in the bundle. | RFC 9172 Section 5.1.2 | M | Y |
| 49 | Missing BIB Policy | If the security policy of a node specifies that a node should have applied integrity to a specific security target and no such BIB is present in the bundle, then the node MUST process this security target in accordance with the security policy. | RFC 9172 Section 5.1.2 | M | Y (11) |
| 50 | Missing BIB Target Removal | If the security policy of a node specifies that a node should have applied integrity to a specific security target and no such BIB is present in the bundle, it is RECOMMENDED that the node remove the security target from the bundle if the security target is not the payload or primary block. | RFC 9172 Section 5.1.2 | O | Y (9) |
| 51 | Missing BIB Payload/Primary | If the security policy of a node specifies that a node should have applied integrity to the payload or primary block, but no such BIB is present in the bundle, the bundle MUST NEITHER be forwarded nor delivered. This action can occur at any node for which policy allows verification of an integrity signature, not just the bundle destination. | RFC 9172 Section 5.1.2 | O | Y (11) |
| 52 | Waypoint BIB Verification | If a receiving node is not the security acceptor of a security operation in a BIB, it MAY attempt to verify the security operation anyway to prevent forwarding corrupt data. | RFC 9172 Section 5.1.2 | O | Y |
| 53 | Verification Failure Policy | If a verification security operation fails, the node SHALL process the security target in accordance with local security policy. | RFC 9172 Section 5.1.2 | M | Y (5) |
| 54 | Waypoint Payload Failure | If a payload integrity check fails at a waypoint, it is RECOMMENDED that it be processed in the same way as a failure of a payload integrity check at the bundle destination. | RFC 9172 Section 5.1.2 | O | Y |
| 55 | Waypoint BIB Retention | If a BIB integrity check passes at waypoint, the node MUST NOT remove the security operation from the BIB prior to forwarding. | RFC 9172 Section 5.1.2 | M | Y |
| 56 | Multi-Op BIB Processing | If a BIB contains multiple security operations, each operation processed by the node MUST be treated as if the security operation has been represented by a single BIB with a single security operation for the purposes of report generation and policy processing. | RFC 9172 Section 5.1.2 | M | Y |
| 57 | No Security On Fragments | A BCB or BIB MUST NOT be added to a bundle if the ‘Bundle is a fragment’ flag is set in the bundle processing control flags field. | RFC 9172 Section 5.2 | M | Y |

#### **Security Contexts (RFC 9173)**

| Item Number | Item | Protocol Feature | Reference | Status | Support |
| :---: | :---- | :---- | :---- | :---: | :---: |
| 58 | BIB-HMAC-SHA2 | Block Integrity Block security context with HMAC-SHA-256, HMAC-SHA-384, and HMAC-SHA-512 variants. | RFC 9173 Section 3 | O | Y (12) |
| 59 | BCB-AES-GCM | Block Confidentiality Block security context with A128GCM and A256GCM variants. | RFC 9173 Section 4 | O | Y (12) |
| 60 | AES Key Wrap | AES key wrap (RFC 3394) of content-encryption and integrity keys, with 128-, 192-, and 256-bit key-encryption keys. | RFC 9173 Sections 3.3.2, 4.3.2 | O | Y |
| 61 | Integrity/AAD Scope Flags | Integrity scope flags (BIB) and AAD scope flags (BCB) covering the primary block, target block header, and security block header. | RFC 9173 Sections 3.3.4, 4.3.4 | O | Y |

### **Notes**

1. The shared-target rule is checked only once a BCB-encrypted BIB is decrypted, since an encrypted BIB's targets are ciphertext to the keyless parse, and then only for security contexts whose operations can share a BCB. BCB-AES-GCM cannot: its IV and any wrapped key are block-level parameters, so the implementation gives each target, a BIB included, its own BCB, and a BCB over a BIB never shares a target with it. That exemption is an interpretation RFC 9172 does not state: read one BCB at a time, §3.8 forbids the "new BCB that targets the existing BIB" that §3.9 offers, which under BCB-AES-GCM is the only layout without key and IV reuse. The exemption stays until the IETF DTN WG rules on §3.8.
2. Not applicable by construction: the implementation never splits a BIB. When a BCB would cover some but not all of a BIB's targets, the encryptor encrypts every other target of that BIB as well, under BCB-AES-GCM each target and the BIB under a BCB of its own, so the first rule of §3.9 applies when those BCBs are read as a set, the same whole-bundle reading as note 1. It refuses to encrypt a block whose BIB also covers the primary block, which no BCB may target. It does not split even a BIB whose integrity scope leaves out the security header, whose moved results would survive: they keep their original source, and RFC 9172 §3.6 names instead the node that inserted the new BIB, which would then appear as the source of a BIB and then a BCB over the same targets, as the final paragraph of §3.9 forbids. Under RFC 9173's default integrity scope a split fails anyway, since the IPPT includes the BIB's block number (RFC 9173 §3.7, step 4).
3. Unrecognised bits in the block processing control flags are preserved rather than zeroed in the canonical form used for integrity and AAD computation. Unknown integrity/AAD scope-flag bits are masked to zero.
4. A BCB is processed on receipt where this node is the operation's acceptor: on a Previous Node or Bundle Age block, which every forwarder replaces, on a Hop Count it can read and check, on a block an Ingress Classifier names it the acceptor of (`MetadataDelta::roles`, per block and per service), and on every block, the payload included, at the bundle's destination, which decrypts the payload as it streams into storage. The operation-specific information is removed before the bundle is first stored. Elsewhere the BCB is carried through unchanged; a node a Classifier names the verifier of the operation authenticates the ciphertext, decrypting it under the key it holds and keeping the plaintext to itself.
5. Failure handling follows the target's block processing control flags (RFC 9171 §5.6 Step 4) rather than a configurable security policy: a failed operation, one this node accepts or is designated to verify but cannot process, for want of a key or in a security context it does not support, and a block that carries no operation of a service its policy expected on it delete the bundle where the flags say so and are otherwise discarded with their operations, an encrypted BIB the removal would shrink going whole with a target the RFCs require gone; the payload and the primary block delete the bundle whatever their flags (per items 37 and 38). A verifier's failed check meets the same fate as an acceptor's.
6. Status reports for a bundle deleted, or a block discarded, for security reasons carry the RFC 9172 §7.1 reason codes: Missing security operation (12) for a block that carries no operation of a service this node's policy expected on it, Unknown security operation (13) for an operation this node accepts in a security context it does not support, Failed security operation (15) for an operation that failed, and Conflicting security operation (16) for an integrity operation on a Previous Node or Bundle Age block inside an encrypted BIB, a combination no valid bundle carries. A missing key is reported as Block unsupported (RFC 9171 §5.6, 11), since RFC 9172 excludes misconfiguration from Failed security operation. Unexpected security operation (14) is emitted only as the deletion reason for a bundle that cannot go to a next hop configured for legacy two-element IPN encoding, because a BPSec operation covers the primary block the re-encode would change, a misconfiguration RFC 9172 §7.1 notes the code may identify; otherwise a node that is neither verifier nor acceptor of an operation leaves it untouched and unreported.
7. A bundle arriving at its destination with a payload BCB for which no decryption key is released (none is configured, or the security source's binding has the verifier role, which releases only verification keys) is deleted at the ingress gate, before anything is stored, reported as Block unsupported: the destination is the acceptor of every operation (RFC 9172 §1.4), and an acceptor that cannot process one has failed to. Nothing waits for a key.
8. Waypoint acceptance is directed by the roles an Ingress Classifier announces, per block and per service (`MetadataDelta::roles`: verifier or acceptor of a block's BCB operation and of its BIB operation), within the key-release policy that binds security-source EID patterns to verifier and acceptor roles. A payload-targeting BCB a Classifier names this node the acceptor of is accepted as the payload streams into storage, as at the destination; a block accepted for confidentiality alone keeps the BIB over it, in plaintext, for the next node.
9. A Classifier that names this node the verifier or acceptor of a service on a block expresses that the block must carry that service: a block that does not is removed, or deletes the bundle where it is the payload or the primary block or its flags say so, with the report Missing security operation, as RFC 9172 §5.1.1 and §5.1.2 recommend of a target the policy expected protected. There is no security policy language beyond that: the Classifier is the embedder's.
10. The acceptor role is determined by RFC 9171 for the per-hop blocks, by RFC 9172 §1.4 at the destination, and otherwise by the roles an Ingress Classifier announces (note 8). An acceptor removes the operations it accepts, a BIB's included: a BIB operation accepted is verified and removed, and a BIB emptied is removed with it. A verifier checks and leaves: by opportunity for every plaintext BIB this node holds a key for, and by designation where a Classifier names it, when a missing key, an unsupported security context, or a BIB encrypted with its target is a failed verification and the target meets the fate of note 5.
11. The primary block's requirement is a configurable validity check: a bundle whose primary block carries neither a CRC nor BIB coverage can be rejected. For any other block, an Ingress Classifier naming this node the verifier or acceptor of its integrity requires a BIB operation on it (note 9).
12. The contexts' handling of a target's CRC is not implemented: no security operation removes a target's CRC at the security source (RFC 9173 §3.8.1, §4.8.1), refuses a target that carries one, or includes one again at a security acceptor (§3.8.2, §4.8.2). The CRC is in neither the IPPT (§3.7) nor the AAD (§4.7), so an operation's result is the same either way; the primary block is immutable (RFC 9171 §4.3.1), and removing or restoring its CRC breaks every operation whose scope includes it; and a CRC the source removes is the one check every node between it and the acceptor can make (RFC 9171 §5.6). A target keeps the CRC its creator gave it, computed over its block-type-specific data as that changes, so a signed or encrypted block from a peer that keeps the CRC is processed, and one from a peer that removed it travels on without one.
