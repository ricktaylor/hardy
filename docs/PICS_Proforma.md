# **Protocol Implementation Conformance Statement (PICS) Proforma**

## **For CCSDS 734.20-O-1: Bundle Protocol Version 7 (Orange Book)**

### **Introduction**

This document provides a Protocol Implementation Conformance Statement (PICS) proforma for the CCSDS Bundle Protocol Version 7, as specified in the experimental Orange Book, CCSDS 734.20-O-1. An implementation claiming conformance to this protocol must satisfy the mandatory requirements specified in this PICS.

To complete this PICS, the implementer is to provide the requested information in the "Implementation Identification" section and to complete the tables in the "PICS Proforma Tables" section by indicating the support for each feature.

### **Implementation Identification**

| Implementation Information | Response |
| :---- | :---- |
| **Supplier** | Aalyria Technologies Inc. |
| **Contact Point for Queries** | Rick Taylor (rtaylor@aalyria.com) |
| **Implementation Name(s) and Version(s)** | Hardy, version 0.1.0, commit bb21b55 |
| **Hardware/Software Environment** | GCP/KVM, Linux 6.1.0-40-cloud-amd64 #1 SMP PREEMPT_DYNAMIC Debian 6.1.153-1 (2025-09-20) x86_64 GNU/Linux |
| **Other Information** |  |

### **Protocol Summary**

| Protocol Information | Response |
| :---- | :---- |
| **Protocol Title** | CCSDS Bundle Protocol Version 7 |
| **Standard Reference** | CCSDS 734.20-O-1, Orange Book, Issue 1 |
| **Date of Standard** | June 2025 (Projected) |
| **Protocol Version** | Version 7 |

### **Instructions for Completing the PICS Proforma**

* **Status Column:**  
  * **M:** Mandatory  
  * **O:** Optional  
  * **O.n:** Optional, but support of at least one of the group of options labeled 'n' is required.  
  * **C:** Conditional  
* **Support Column:**  
  * **Y:** Yes, the feature is implemented.  
  * **N:** No, the feature is not implemented.  
  * **N/A:** Not applicable.

### **PICS Proforma Tables**

#### **Basic Requirements**

| Item Number | Item | Protocol Feature | Reference | Status | Support |
| :---- | :---- | :---- | :---- | :---- | :---- |
| 1 | BP Formatting | Formats bundles as BPv7 per RFC 9171\. | This doc: 3.1; RFC 9171 Sec 4 (exceptions apply) | M | Y |
| 2 | Previous Node Receive | Recognizes, parses, and acts on the previous node extension block. | RFC 9171 sec 4.4.1 | M | Y |
| 3 | Previous Node Produce | Create previous node extension block. | RFC 9171 sec 4.4.1 | O | Y |
| 4 | Bundle Age Receive | Recognizes, parses, and acts on the bundle age extension block. | RFC 9171 sec 4.4.2 | M | Y |
| 5 | Bundle Age Produce | Create bundle age extension block. (Mandatory if bundle creation time \= 0; Optional otherwise). | RFC 9171 sec 4.4.2 | C | Y |
| 6 | Hop Count Receive | Recognizes, parses, and acts on the hop count extension block. | RFC 9171 sec 4.4.3 | M | Y |
| 7 | Hop Count Produce | Create hop count extension block. | RFC 9171 sec 4.4.3 | O | Y |
| 8 | BPv7 | Identifies bundles as version 7 in the primary block. | RFC 9171 sec 9.2 | M | Y |
| 9 | IPN naming | Support for the ipn URI scheme. | This doc: 3.2.1; RFC 9171 sec 4.2.5.1.2 | M | Y |
| 10 | Null endpoint | Support for the null endpoint. | This doc: 3.2.2; RFC 9171 sec 4.2.5.1.1 | O | Y |
| 11 | IPN Node No | Use ipn node numbers assigned by SANA. | This doc: 3.2.3 | M | Y |
| 12 | IPN Service No | Use ipn service numbers assigned by IANA/SANA. | This doc: 3.2.4 | M | Y |
| 13 | Bundle Creation Metadata | Bundle creation timestamp and sequence number assigned when ADU is accepted for transmission. | This doc: 3.3.1 | M | Y |
| 14 | Bundle Send Request | The combination of source node ID and creation timestamp is returned to the sending application. | This doc: 3.3.2 | M | Y |
| 15 | Source Node ID | The source node IDs for all non-anonymous bundles' sources shall have the same node number. | This doc: 3.3.3 | M | Y |
| 16 | Registration Constraints | All endpoints in which a node is registered shall have the same node number. | This doc: 3.4 | M | Y |
| 17 | BPA Node Numbers | The node number is the same as is encoded in all the endpoints in which the node is registered. | This doc: 3.5.1 | M | Y |
| 18 | BPA Endpoint Registration | No two BPAs shall register in endpoints whose EIDs have the same node number. | This doc: 3.5.2 | M | N/A |
| 19 | Minimum Bundle Size | Supports processing of bundles whose total size is no less than 10\*2^20 bytes (10 MB). | This doc: 3.6 | M | Y |
| 20 | BPSec | BPSec is not required for implementations of BPv7. | This doc: 3.7; RFC 9172 | O | Y |
| 21 | Service Interface | Supports the service interface in section 4\. | This doc: section 4 | M | Y |
| 22 | BP Node | Services that BP needs from an external source. | This doc: section 5 | M | Y |
| 23 | TCP CLA | Implements bundle encapsulation in TCP segments. | This doc: B2.1.2 | O.1 | Y |
| 24 | LTP CLA | Implements bundle encapsulation in LTP blocks. | This doc: B2.1.4 | O.1 | N |
| 25 | UDP CLA | Implements bundle encapsulation in UDP datagrams. | This doc: B2.1.3 | O.1 | N |
| 26 | Space Packets CLA | Implements encapsulation of bundles in Space Packets. | This doc: B2.1.5 | O.1 | N |
| 27 | EPP CLA | Implements encapsulation of bundles in encapsulation packets. | This doc: B2.1.6 | O.1 | N |
| 28 | BP Managed Information | Implements the BP managed information described in annex C. | This doc, annex C | M | N |
| 29 | BP Data Structures | Follows RFC 9171 rules for data structures. | RFC 9171 Sec 4.2 | M | Y |
| 30 | Block Structures | Follows RFC 9171 rules for details in blocks. | RFC 9171 Sec 4.3 | M | Y |
| 31 | Extension Blocks | Follows RFC 9171 rules for details in extension blocks. | RFC 9171 Sec 4.4 | M | Y |
| 32 | Generation of Admin Records | Follows RFC 9171 rules for generation of administrative records ("off by default"). | RFC 9171 Sec 5.1 | M | Y |
| 33 | Bundle Transmission | Follows RFC 9171 procedures for bundle transmission. | RFC 9171 Sec 5.2 | M | Y |
| 34 | Forwarding Contraindicated | Follows RFC 9171 procedures when forwarding is contraindicated. | RFC 9171 Sec 5.3 | M | Y |
| 35 | Forwarding Failed | Follows RFC 9171 procedures when forwarding a bundle fails. | RFC 9171 Sec 5.4 | M | Y |
| 36 | Forwarding Failed Return | Follows RFC 9171 procedures when forwarding fails to forward bundle to previous node. | RFC 9171 Sec 5.4.2 | O | Y |
| 37 | Bundle Expiration | Follows RFC 9171 procedures when a bundle expires. | RFC 9171 Sec 5.5 | M | Y |
| 38 | Bundle Reception | Follows RFC 9171 procedures when receiving a bundle. | RFC 9171 Sec 5.6 | M | Y |
| 39 | Local Bundle Delivery | Follows RFC 9171 procedures when delivering a bundle to the AA. | RFC 9171 Sec 5.7 | M | Y |
| 40 | Bundle Fragmentation | Implementation supports fragmentation of bundles per RFC 9171\. | RFC 9171 Sec 5.8 | O | N |
| 41 | Fragmentation Procedures | Follows RFC 9171 procedures when fragmenting a bundle. (Mandatory if Item 31 is true). | RFC 9171 Sec 5.8 | C | N |
| 42 | ADU Reassembly | Follows RFC 9171 procedures when reassembling an ADU. | RFC 9171 Sec 5.9 | M | Y |
| 43 | Bundle Deletion Report | Generates bundle deletion status report. | RFC 9171 Sec 5.10 | O | Y |
| 44 | Bundle Deletion Constraints | Removes retention constraints when deleting a bundle. | RFC 9171 Sec 5.10 | M | Y |
| 45 | Discarding a Bundle | Follows RFC 9171 procedures when discarding a bundle. | RFC 9171 Sec 5.11 | M | Y |
| 46 | Canceling a Transmission | Follows RFC 9171 procedures when canceling an initial transmission. | RFC 9171 Sec 5.12 | O | Y |
| 47 | Administrative Records | Formats administrative records per RFC 9171\. (Mandatory if item 49 is true). | RFC 9171 sec 6.1 | C | Y |
| 48 | Bundle Status Reports | Formats status reports per RFC 9171\. (Mandatory if item 49 is true). | RFC 9171 sec 6.1.1 | C | Y |
| 49 | Generating Admin Records | Follows RFC 9171 procedures when generating an administrative record. | RFC 9171 Sec 6.2 | O | Y |
