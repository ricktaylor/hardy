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
| 29 | BCB Acceptor Processing | If the receiving node is the security acceptor for any of the security operations in the BCB, the node MUST process those operations and remove any operation-specific information from the BCB prior to delivering data to an application at the node or forwarding the bundle. | RFC 9172 Section 5.1.1 | M | P (4) |
| 30 | BCB Failure Policy | If processing a BCB security operation fails, the target SHALL be processed according to the security policy. | RFC 9172 Section 5.1.1 | M | P (5) |
| 31 | BCB Failure Report | If processing a BCB security operation fails, a bundle status report indicating the failure MAY be generated. | RFC 9172 Section 5.1.1 | O | Y (6) |
| 32 | Empty BCB Removal | When all security operations for a BCB have been removed from the BCB, the BCB MUST be removed from the bundle. | RFC 9172 Section 5.1.1 | M | Y |
| 33 | Destination Decryption | If the receiving node is the destination of the bundle, the node MUST decrypt any BCBs remaining in the bundle. | RFC 9172 Section 5.1.1 | M | P (7) |
| 34 | Waypoint BCB Policy | If the receiving node is not the destination of the bundle, the node MUST process the BCB if directed to do so as a matter of security policy. | RFC 9172 Section 5.1.1 | M | P (8) |
| 35 | Missing BCB Policy | If the security policy of a node specifies that a node should have applied confidentiality to a specific security target and no such BCB is present in the bundle, then the node MUST process this security target in accordance with the security policy. | RFC 9172 Section 5.1.1 | M | N (9) |
| 36 | Payload Removal Discard | If the payload block is removed as a result of security processing, the bundle MUST be discarded. | RFC 9172 Section 5.1.1 | M | Y |
| 37 | Payload Decrypt Failure | If an encrypted payload block cannot be decrypted (i.e., the ciphertext cannot be authenticated), then the bundle MUST be discarded and processed no further. | RFC 9172 Section 5.1.1 | M | Y |
| 38 | Non-Payload Decrypt Failure | If an encrypted security target other than the payload block cannot be decrypted, then the associated security target and all security blocks associated with that target MUST be discarded and processed no further. | RFC 9172 Section 5.1.1 | M | Y |
| 39 | Security Deletion Reports | As a result of security operations, if a block is deleted from a bundle or if a bundle is dropped, requested status reports (see reference [2]) MAY be generated to reflect bundle or block deletion. | RFC 9172 Section 5.1.1 | O | Y (6) |
| 40 | Plaintext Replacement | When a BCB is decrypted, the recovered plaintext for each security target MUST replace the ciphertext in each of the security targets’; block-type-specific data fields. | RFC 9172 Section 5.1.1 | M | Y |
| 41 | Byte String Reframing | If the plaintext is of a different size than the ciphertext, the framing of the CBOR byte string of this field MUST be updated to ensure this field remains a valid CBOR byte string. | RFC 9172 Section 5.1.1 | M | Y |
| 42 | Multi-Op BCB Processing | If a BCB contains multiple security operations, each operation processed by the node MUST be treated as if the security operation has been represented by a single BCB with a single security operation for the purposes of report generation and policy processing. | RFC 9172 Section 5.1.1 | M | Y |
| 43 | BIB Acceptor Determination | If a received bundle contains a BIB, the receiving node MUST determine whether it is the security acceptor for any of the security operations in the BIB. | RFC 9172 Section 5.1.2 | M | P (10) |
| 44 | BIB Acceptor Processing | If the receiving node is the security acceptor for any security operations in a BIB, the node MUST process those operations and remove any operation-specific information from the BIB prior to delivering data to an application at the node or forwarding the bundle. | RFC 9172 Section 5.1.2 | M | N (10) |
| 45 | BIB Failure Policy | If processing a BIB security operation fails, the target SHALL be processed according to the security policy. | RFC 9172 Section 5.1.2 | M | P (5) |
| 46 | BIB Failure Report | If processing a BIB security operation fails, a bundle status report indicating the failure MAY be generated. | RFC 9172 Section 5.1.2 | O | Y (6) |
| 47 | Empty BIB Removal | When all security operations for a BIB have been removed from the BIB, the BIB MUST be removed from the bundle. | RFC 9172 Section 5.1.2 | M | Y |
| 48 | BIB Under BCB Not Processed | A BIB MUST NOT be processed if the security target of the BIB is also the security target of a BCB in the bundle. | RFC 9172 Section 5.1.2 | M | Y |
| 49 | Missing BIB Policy | If the security policy of a node specifies that a node should have applied integrity to a specific security target and no such BIB is present in the bundle, then the node MUST process this security target in accordance with the security policy. | RFC 9172 Section 5.1.2 | M | P (11) |
| 50 | Missing BIB Target Removal | If the security policy of a node specifies that a node should have applied integrity to a specific security target and no such BIB is present in the bundle, it is RECOMMENDED that the node remove the security target from the bundle if the security target is not the payload or primary block. | RFC 9172 Section 5.1.2 | O | N (9) |
| 51 | Missing BIB Payload/Primary | If the security policy of a node specifies that a node should have applied integrity to the payload or primary block, but no such BIB is present in the bundle, the bundle MUST NEITHER be forwarded nor delivered. This action can occur at any node for which policy allows verification of an integrity signature, not just the bundle destination. | RFC 9172 Section 5.1.2 | O | P (11) |
| 52 | Waypoint BIB Verification | If a receiving node is not the security acceptor of a security operation in a BIB, it MAY attempt to verify the security operation anyway to prevent forwarding corrupt data. | RFC 9172 Section 5.1.2 | O | Y |
| 53 | Verification Failure Policy | If a verification security operation fails, the node SHALL process the security target in accordance with local security policy. | RFC 9172 Section 5.1.2 | M | P (5) |
| 54 | Waypoint Payload Failure | If a payload integrity check fails at a waypoint, it is RECOMMENDED that it be processed in the same way as a failure of a payload integrity check at the bundle destination. | RFC 9172 Section 5.1.2 | O | Y |
| 55 | Waypoint BIB Retention | If a BIB integrity check passes at waypoint, the node MUST NOT remove the security operation from the BIB prior to forwarding. | RFC 9172 Section 5.1.2 | M | Y |
| 56 | Multi-Op BIB Processing | If a BIB contains multiple security operations, each operation processed by the node MUST be treated as if the security operation has been represented by a single BIB with a single security operation for the purposes of report generation and policy processing. | RFC 9172 Section 5.1.2 | M | Y |
| 57 | No Security On Fragments | A BCB or BIB MUST NOT be added to a bundle if the ‘Bundle is a fragment’ flag is set in the bundle processing control flags field. | RFC 9172 Section 5.2 | M | Y |

#### **Security Contexts (RFC 9173)**

| Item Number | Item | Protocol Feature | Reference | Status | Support |
| :---: | :---- | :---- | :---- | :---: | :---: |
| 58 | BIB-HMAC-SHA2 | Block Integrity Block security context with HMAC-SHA-256, HMAC-SHA-384, and HMAC-SHA-512 variants. | RFC 9173 Section 3 | O | Y |
| 59 | BCB-AES-GCM | Block Confidentiality Block security context with A128GCM and A256GCM variants. | RFC 9173 Section 4 | O | Y |
| 60 | AES Key Wrap | AES key wrap (RFC 3394) of content-encryption and integrity keys, with 128-, 192-, and 256-bit key-encryption keys. | RFC 9173 Sections 3.3.2, 4.3.2 | O | Y |
| 61 | Integrity/AAD Scope Flags | Integrity scope flags (BIB) and AAD scope flags (BCB) covering the primary block, target block header, and security block header. | RFC 9173 Sections 3.3.4, 4.3.4 | O | Y |

### **Notes**

1. The shared-target rule is only checked for security contexts whose operations can share a BCB. BCB-AES-GCM emits a single-target BCB per operation (each needs a distinct IV), so a BCB encrypting a BIB can never literally share a target with it; the check is therefore deliberately not enforced on parse for this context.
2. Not applicable by construction: rather than splitting a partially overlapped BIB (which would require the integrity keys), the implementation widens the BCB to cover every target of the BIB, so the partial-overlap case cannot arise.
3. Unrecognised bits in the block processing control flags are preserved rather than zeroed in the canonical form used for integrity and AAD computation. Unknown integrity/AAD scope-flag bits are masked to zero.
4. BCBs targeting extension blocks are processed on receipt. BCBs targeting the payload block are deliberately carried through the node and decrypted only on delivery to the local application, so operation-specific information is removed at delivery rather than at reception.
5. Failure handling is fixed rather than driven by configurable security policy: an integrity or payload-decryption failure discards the bundle, and a non-payload decryption failure removes the target and its associated security blocks (per items 37 and 38).
6. Status reports are generated when a bundle is dropped for security reasons, but with the generic "Block unintelligible" reason code; the RFC 9172 security reason codes (12–16) are defined but not yet emitted.
7. A bundle arriving at its destination with a payload BCB for which no key is available is currently held pending rather than processed; resolution of this case is an open work item.
8. Waypoint acceptance is directed by key-release policy (per-security-source EID patterns bound to verifier/acceptor roles) and works for extension-block targets; payload-targeting BCBs cannot be accepted at a waypoint because payload decryption occurs only at delivery (see note 4).
9. No "protection required" security policy is implemented: there is no mechanism to specify that a target should have been protected by a BCB or BIB, and consequently no target removal for missing protection.
10. The security acceptor role is determined implicitly by key release, and configuration distinguishes verifier and acceptor roles, but the two currently behave identically for BIBs: security operations are not removed from a BIB after successful verification at an acceptor (only BCBs are removed).
11. Implemented for the primary block only: a validity filter can reject bundles whose primary block carries neither a CRC nor BIB coverage. There is no general mechanism to require integrity protection on the payload or other targets.
