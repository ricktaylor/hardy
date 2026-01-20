# Unit Test Plan: Bundle Protocol Security (BPSec)

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Security (Integrity & Confidentiality) |
| **Module** | `hardy-bpv7` (Security Submodule) |
| **Requirements Ref** | `DTN-HLR_v1` (REQ-2), `DTN-LLR_v1.1` (Section 2.0) |
| **Standard Ref** | RFC 9172 (BPSec Core), RFC 9173 (BPSec Contexts) |
| **Test Suite ID** | UTP-BPSEC-01 |

## 1. Introduction

This document details the unit testing strategy for the BPSec implementation within `hardy`. This functional area is responsible for ensuring bundles are authenticated (BIB) and encrypted (BCB) to prevent tampering or eavesdropping in transit.

**Scope:**

* **Block Integrity Block (BIB):** Parsing, generation (Sign), and validation (Verify) of signatures/HMACs.

* **Block Confidentiality Block (BCB):** Parsing, generation (Encrypt), and encryption/decryption of payloads.

* **RFC 9173 Contexts:** Verification of standard algorithms (HMAC-SHA2, AES-GCM).

* **Factories:** Verification of `Signer` and `Encryptor` structs for programmatic block generation.

* **Abstract Security Block (ASB):** Verification of generic CBOR structure.

## 2. Requirements Mapping

The following requirements from **DTN-LLR_v1.1** are verified by this plan:

| LLR ID | Description | RFC Ref |
 | ----- | ----- | ----- |
| **2.1.1** | Parser must identify BIB (Type 11) and BCB (Type 12) blocks. | RFC 9172 Sec 3.1 |
| **2.1.2** | Correctly remove BPSec target info when targeted block is removed. | RFC 9172 Sec 3.2 |
| **2.1.3** | Validate that Fragmented bundles do NOT contain BPSec extension blocks. | RFC 9172 Sec 3.8 |
| **2.2.1** | Support BIB-HMAC-SHA2 context with 256-bit hash. | RFC 9173 Sec 3 |
| **2.2.2** | Support BIB-HMAC-SHA2 context with 384-bit hash. | RFC 9173 Sec 3 |
| **2.2.3** | Support BIB-HMAC-SHA2 context with 512-bit hash. | RFC 9173 Sec 3 |
| **2.2.4** | Support key-wrap function on HMAC keys. | RFC 9173 Sec 3.3 |
| **2.2.5** | Support BCB-AES-GCM context with 128-bit key. | RFC 9173 Sec 4 |
| **2.2.6** | Support BCB-AES-GCM context with 256-bit key. | RFC 9173 Sec 4 |
| **2.2.7** | Support key-wrap function on AES keys. | RFC 9173 Sec 4.3 |

## 3. Unit Test Cases

The following scenarios are verified by the unit tests located in `hardy-bpv7/src/bpsec/`.

### 3.1 Abstract Security Block (ASB) Parsing (LLR 2.1.1)

*Objective: Verify the generic outer shell of security blocks.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **BIB Identification** | Verify parsing of Block Type 11. | TODO | Block Type `11` | `SecurityType::BIB` |
| **BCB Identification** | Verify parsing of Block Type 12. | TODO | Block Type `12` | `SecurityType::BCB` |
| **Target Reference** | Verify logic to find the "Target" block (e.g., Payload). | TODO | Targets: `[1]` (Payload) | Target Reference resolved to Block 1 |
| **Multi-Target Handling** | Verify strict adherence to target multiplicity rules. | TODO | Multiple targets (if supported) | Parsed correctly or Error (profile dependent) |
| **ASB CBOR Parsing** | Verify parsing of the Abstract Syntax Block structure. | TODO | Valid ASB CBOR | `AbstractSyntaxBlock` struct |

### 3.2 Block Integrity (BIB) Logic (LLR 2.2.1 - 2.2.3)

*Objective: Verify that signatures are generated and verified correctly.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Sign Generation** | Generate a MAC for a specific payload. | TODO | Key: `secret`, Data: `payload` | Result: Valid `target_ciphersuite_params` |
| **Verify Valid** | Verify a valid signature/MAC matches the payload. | TODO | Key: `secret`, Data: `payload` | Result: `Ok` |
| **Verify Tampered** | Modify one byte of payload after signing. | TODO | Key: `secret`, Data: `pAylod` | Error: `IntegrityCheckFailed` |
| **Security Source** | Verify "Security Source" EID parsing. | TODO | Source: `ipn:5.5` | `security_src == ipn:5.5` |

### 3.3 Block Confidentiality (BCB) Logic (LLR 2.2.5 - 2.2.6)

*Objective: Verify encryption and decryption flow.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Encrypt Payload** | Encrypt plaintext payload into ciphertext. | TODO | Plaintext: `secret` | Target block contains Ciphertext + Auth Tag |
| **Decrypt Payload** | Verify decryption replaces payload content. | TODO | Ciphertext from above | Target block contains "secret" |
| **Key ID Mismatch** | Attempt decrypt with wrong key index. | TODO | KeyID: `2` (Stored is `1`) | Error: `KeyNotFound` |

### 3.4 RFC 9173 Security Contexts (LLR 2.2.1 - 2.2.7)

*Objective: Verify specific cryptographic algorithms defined in RFC 9173.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **HMAC-SHA256 (ID 1)** | Verify BIB Context ID 1 with 32-byte key. (LLR 2.2.1) | TODO | Ciphersuite: `1`, Key: 32B | Result: `Ok` (Valid SHA256 MAC) |
| **HMAC-SHA384 (ID 2)** | TODO: Verify BIB Context ID 2 with 48-byte key. (LLR 2.2.2) | `src/bpsec/rfc9173/bib_hmac.rs` | Ciphersuite: `2`, Key: 48B | Result: `Ok` |
| **HMAC-SHA384 (ID 2)** | Verify BIB Context ID 2 with 48-byte key. (LLR 2.2.2) | TODO | Ciphersuite: `2`, Key: 48B | Result: `Ok` |
| **AES-GCM-128 (ID 1)** | Verify BCB Context ID 1 with 16-byte key. (LLR 2.2.5) | TODO | Ciphersuite: `1`, Key: 16B | Result: `Ok` (Valid AES-GCM Encrypt) |
| **AES-GCM-256 (ID 3)** | Verify BCB Context ID 3 with 32-byte key. (LLR 2.2.6) | TODO | Ciphersuite: `3`, Key: 32B | Result: `Ok` (Valid AES-GCM Encrypt) |
| **IV Randomness** | Verify IVs are unique/random for AES-GCM encryption. | TODO | 2 Sequential Encryptions | `IV1 != IV2` |
| **Wrapped Key Unwrap** | Verify unwrapping of a session key using a KEK. (LLR 2.2.4, 2.2.7) | TODO | Wrapped Key Material | Result: `Ok(UnwrappedKey)` |
| **Wrapped Key Fail** | Verify failure when unwrapping a corrupted key blob. | TODO | Corrupted Wrapped Key | Error: `KeyUnwrapFailed` |

### 3.5 Security Factories (Signer & Encryptor)

*Objective: Verify API structs for creating security blocks on existing bundles.*

| Test Scenario | Description | Source File | Input | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Signer - Add BIB** | Use `Signer` to add a BIB to a payload block. | TODO | `Signer::new(HMAC_SHA256, key).sign(bundle, Target::Payload)` | Bundle contains new BIB (Type 11) targeting Payload. |
| **Signer - Invalid Target** | Attempt to sign a non-existent block. | TODO | Target: `BlockIndex(99)` | Error: `TargetNotFound` |
| **Encryptor - Apply BCB** | Use `Encryptor` to encrypt a payload. | TODO | `Encryptor::new(AES_GCM, key).encrypt(bundle, Target::Payload)` | Bundle contains new BCB (Type 12); Payload block replaced with Ciphertext. |
| **Encryptor - Re-encrypt** | Attempt to encrypt an already encrypted target (if unsupported). | TODO | Target: Block already listed in another BCB | Error: `TargetAlreadyEncrypted` (Profile Dependent) |

### 3.6 Edge Cases & Constraints (LLR 2.1.2, 2.1.3)

| Test Scenario | Description | Source File | Input | Expected Output |
| ----- | ----- | ----- | ----- | ----- |
| **Target Removal** | Verify BPSec info is removed when target block is deleted. (LLR 2.1.2) | TODO | Bundle with BIB targeting deleted block | BIB removed/updated |
| **Fragmentation Check** | Verify fragmented bundles cannot contain BPSec blocks. (LLR 2.1.3) | TODO | Fragmented Bundle + BIB | Error: `InvalidFlags` |

### 3.7 RFC 9173 Appendix A Compliance (Standard Vectors)

*Objective: Verify bit-exact matching of all 4 standard RFC 9173 worked examples.*

| Test Scenario | Description | Source File | RFC Ref | Expected Output |
 | ----- | ----- | ----- | ----- | ----- |
| **Appendix A.1 (BIB)** | **BIB-HMAC-SHA2 Example 1**<br>Verify generation of signature matches RFC hex dump. | `src/bpsec/rfc9173/mod.rs` | Appx A.1 | Generated MAC matches `0x5d...` |
| **Appendix A.2 (BCB)** | **BCB-AES-GCM Example 1**<br>Verify encryption of payload matches RFC hex dump. | `src/bpsec/rfc9173/mod.rs` | Appx A.2 | Ciphertext matches `0x5468...`<br>Auth Tag matches `0x...` |
| **Appendix A.3 (BCB)** | **BCB-AES-GCM Example 2**<br>Verify encryption using 256-bit key. | `src/bpsec/rfc9173/mod.rs` | Appx A.3 | Ciphertext matches `0x...` |
| **Appendix A.4 (BCB)** | **BCB-AES-GCM Example 3**<br>Verify encryption with Additional Authenticated Data (AAD) or variant. | TODO | Appx A.4 | Ciphertext/Tag match RFC vectors. |

## 4. Execution & Pass Criteria

* **Command:** `cargo test -p hardy-bpv7 --lib`

* **Pass Criteria:** All tests listed above must return `ok`.

* **Coverage Target:** > 90% line coverage for `src/bpsec/`.
