# Component Test Plan: BPv7 Module (via CLI Driver)

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Bundle Protocol Agent Core Logic |
| **Component** | `hardy-bpv7` Library |
| **Test Driver** | `hardy-bpv7-tools` (Binary: `bundle`), `jq` |
| **Requirements Ref** | `DTN-LLR_v1.1` |
| **Standard Ref** | RFC 9171 (BPv7), RFC 9172 (BPSec), RFC 9173 (Contexts) |
| **Scope** | Verification of Library Parsing, Serialization, and Security Logic using the CLI harness. |
| **Test Suite ID** | COMP-BPV7-CLI-01 |

## 1. Introduction

This document details the component testing strategy for the `hardy-bpv7` library. We utilize the `hardy-bpv7-tools` CLI binary (`bundle`) as a "Test Driver" to stimulate the library's internal functions (Builder, Editor, Serializer, Validator).

The tests are organized into **Functional Suites**, where each suite targets a specific capability of the Bundle Protocol implementation.

**Prerequisites:**

* **Driver:** `bundle` binary (from `hardy-bpv7-tools`).

* **Key Material:** JSON keyfile containing valid HMAC and AES keys.

* **Tools:** `jq` (for JSON assertions).

* **Environment:** Standard shell (Bash/PowerShell).

## 2. Requirements Mapping

The following requirements from **DTN-LLR_v1.1** are verified by this plan:

| LLR ID | Description |
| ----- | ----- |
| **1.1.1** | Compliant with all mandatory requirements of CCSDS Bundle Protocol. |
| **1.1.15** | Parser must indicate that the Primary Block is valid. |
| **1.1.16** | Parser must indicate that all recognised extension blocks are valid. |
| **1.1.17** | Parser must indicate that the Bundle as a whole is valid. |
| **1.1.19** | Parser must parse/validate extension blocks specified in RFC 9171. |
| **1.1.21** | Parser must parse and validate all CRC values. |
| **1.1.22** | Parser must support all CRC types specified in RFC 9171. |
| **1.1.25** | Generator must create valid, canonical CBOR encoded bundles. |
| **1.1.27** | Generator must apply required CRC values to all bundles. |
| **1.1.28** | Generator must apply required CRC values to all extension blocks. |
| **1.1.29** | Generator must allow caller to specify the CRC type (16/32/None). |
| **1.1.30** | Processing must enforce bundle rewriting rules when discarding unrecognised blocks. |
| **1.1.31** | Processing may rewrite non-canonical bundles into canonical form (policy allow). |
| **1.1.33** | Processing must use Bundle Age block for expiry if Creation Time is zero. |
| **1.1.34** | Processing must process and act on Hop Count extension block. |
| **2.1.1** | Validate BIB/BCB blocks according to abstract syntax (RFC 9172). |
| **2.2.1** | Support BIB-HMAC-SHA2 context with 256-bit hash. |
| **2.2.2** | Support BIB-HMAC-SHA2 context with 384-bit hash. |
| **2.2.3** | Support BIB-HMAC-SHA2 context with 512-bit hash. |
| **2.2.4** | Support key-wrap function on HMAC keys. |
| **2.2.5** | Support BCB-AES-GCM context with 128-bit key. |
| **2.2.6** | Support BCB-AES-GCM context with 256-bit key. |
| **2.2.7** | Support key-wrap function on AES keys. |

## 3. Test Data Setup

```bash
# Ensure valid keyfile exists at ./test_data/test-keys.json
export BUNDLE="cargo run --quiet --package hardy-bpv7-tools --bin bundle --"
export KEYS="./test_data/test-keys.json"

function bundle_jq() {
  $BUNDLE dump "$1" | jq -e "$2"
}
export -f bundle_jq
```

## 4. Functional Test Suites

### Suite 1: Bundle Creation (LLR 1.1.1, 1.1.25)

*Objective: Verify the Builder logic and Primary Block serialization.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **CREATE-01** | **Minimal Bundle**<br><br>Create a bundle with only mandatory fields (Source, Dest, Lifetime). | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 -o ./test_data/create_01.bundle` | `bundle_jq ./test_data/create_01.bundle '.primary.version == 7 and .primary.source == "ipn:1.1" and .primary.destination == "ipn:2.1"'` |
| **CREATE-02** | **Payload from Stdin**<br><br>Stream data into the payload during creation. | `echo "StreamData" \| $BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --payload - -o ./test_data/create_02.bundle` | `bundle_jq ./test_data/create_02.bundle '.payload_block.data_base64 == "U3RyZWFtRGF0YQo="'` |
| **CREATE-03** | **Payload from File**<br><br>Load payload data from an existing file. | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --payload ./test_data/image.png -o ./test_data/create_03.bundle` | `bundle_jq ./test_data/create_03.bundle '.payload_block.length > 0'` |
| **CREATE-04** | **No CRC**<br><br>Create a bundle with no CRC. (LLR 1.1.29) | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --crc-type none -o ./test_data/create_04.bundle` | `bundle_jq ./test_data/create_04.bundle '.primary.crc_type == "none"'` |
| **CREATE-05** | **CRC Type**<br><br>Create a bundle with a non-default CRC type. (LLR 1.1.22, 1.1.29) | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --crc-type crc16 -o ./test_data/create_05.bundle` | `bundle_jq ./test_data/create_05.bundle '.primary.crc_type == "crc16"'` |
| **CREATE-06** | **Zero Timestamp**<br><br>Create a bundle with DTN Time 0. (LLR 1.1.33) | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --creation-time 0 --bundle-age 5000 -o ./test_data/create_06.bundle` | `bundle_jq ./test_data/create_06.bundle '.primary.creation_timestamp == 0'` |

### Suite 2: Extension Blocks (LLR 1.1.19, 1.1.34)

*Objective: Verify the Editor logic and handling of standard Extension Blocks.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **EXT-01** | **Add Hop Count**<br><br>Insert a Hop Count block limit. | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --hop-count 5 -o ./test_data/ext_01.bundle` | `bundle_jq ./test_data/ext_01.bundle 'any(.extension_blocks[]; .block_type == 10 and .limit == 5)'` |
| **EXT-02** | **Add Previous Node**<br><br>Insert a Previous Node EID block. | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --previous-node ipn:9.9 -o ./test_data/ext_02.bundle` | `bundle_jq ./test_data/ext_02.bundle 'any(.extension_blocks[]; .block_type == 6 and .eid == "ipn:9.9")'` |
| **EXT-03** | **Report-To EID**<br><br>Set the Report-To field in the Primary Block. | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --report-to ipn:3.3 -o ./test_data/ext_03.bundle` | `bundle_jq ./test_data/ext_03.bundle '.primary.report_to == "ipn:3.3"'` |
| **EXT-04** | **Bundle Age**<br><br>Insert a Bundle Age block. (LLR 1.1.33) | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --bundle-age 5000 -o ./test_data/ext_04.bundle` | `bundle_jq ./test_data/ext_04.bundle 'any(.extension_blocks[]; .block_type == 7 and .age == 5000)'` |

### Suite 3: Integrity Operations (BIB) (LLR 2.1.1, 2.2.1, 2.2.3)

*Objective: Verify the Signer/Verifier logic and RFC 9172/9173 compliance.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **SIGN-01a** | **Sign Payload**<br><br>Apply HMAC-SHA256 to the payload block. | `$BUNDLE sign ./test_data/create_01.bundle -o ./test_data/sign_01a.bundle -s ipn:1.1 --keys $KEYS --kid "hmackey" --target payload` | `bundle_jq ./test_data/sign_01a.bundle 'any(.extension_blocks[]; .block_type == 11 and .security_target[0] == 1 and .cipher_suite_id == 1)'` |
| **SIGN-01b** | **Sign Payload**<br><br>Apply HMAC-SHA256 to the payload block. | `$BUNDLE sign ./test_data/create_01.bundle -o ./test_data/sign_01b.bundle -s ipn:1.1 --keys $KEYS --kid "hmackey" --block-id 1` | `bundle_jq ./test_data/sign_01b.bundle 'any(.extension_blocks[]; .block_type == 11 and .security_target[0] == 1 and .cipher_suite_id == 1)'` |
| **SIGN-02** | **Sign Extension Block**<br><br>Apply integrity to a specific extension block (e.g., Hop Count). | `$BUNDLE sign ./test_data/ext_01.bundle -o ./test_data/sign_02.bundle -s ipn:1.1 --keys $KEYS --kid "hmackey" --block-id 2` | `bundle_jq ./test_data/sign_02.bundle 'any(.extension_blocks[]; .block_type == 11 and .security_target[0] == 2)'` |
| **SIGN-03** | **Verify Valid**<br><br>Verify a correctly signed bundle. | `$BUNDLE verify ./test_data/sign_01a.bundle --keys $KEYS` | Returns success/valid status.<br><br>Verifies HMAC calculation matches. |
| **SIGN-04** | **Verify Tampered**<br><br>Verify a bundle with modified payload bytes. | `[External Mod]`<br><br>`$BUNDLE verify ./test_data/tampered.bundle --keys $KEYS` | Returns failure.<br><br>Specific error indicating integrity mismatch (not parsing error). |
| **SIGN-05** | **Alternate Ciphersuite**<br><br>Apply HMAC-SHA384 to the payload. (LLR 2.2.2) | `$BUNDLE sign ./test_data/create_01.bundle -o ./test_data/sign_05.bundle -s ipn:1.1 --keys $KEYS --kid "hmackey_384" --target payload` | `bundle_jq ./test_data/sign_05.bundle 'any(.extension_blocks[]; .block_type == 11 and .cipher_suite_id == 2)'` |
| **SIGN-06** | **Verify Extension Block**<br><br>Verify the integrity of a signed extension block. | `$BUNDLE verify ./test_data/sign_02.bundle --keys $KEYS --block-id 2` | Returns success/valid status.<br><br>Verifies HMAC calculation for Block 2. |
| **SIGN-07** | **HMAC-SHA512**<br><br>Apply HMAC-SHA512 to the payload. (LLR 2.2.3) | `$BUNDLE sign ./test_data/create_01.bundle -o ./test_data/sign_07.bundle -s ipn:1.1 --keys $KEYS --kid "hmackey_512" --target payload` | `bundle_jq ./test_data/sign_07.bundle 'any(.extension_blocks[]; .block_type == 11 and .cipher_suite_id == 3)'` |
| **SIGN-08** | **HMAC Key Wrap**<br><br>Sign payload using a session key wrapped with a KEK. (LLR 2.2.4) | `$BUNDLE sign ./test_data/create_01.bundle -o ./test_data/sign_08.bundle -s ipn:1.1 --keys $KEYS --kid "hmackey_kw" --target payload` | `bundle_jq ./test_data/sign_08.bundle 'any(.extension_blocks[]; .block_type == 11 and .cipher_suite_id == 1)'` |

### Suite 4: Confidentiality Operations (BCB) (LLR 2.1.1, 2.2.6)

*Objective: Verify the Encryptor/Decryptor logic and AES-GCM implementation.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **ENC-01a** | **Encrypt Payload**<br><br>Apply AES-GCM encryption to the payload. | `$BUNDLE encrypt ./test_data/create_01.bundle -o ./test_data/enc_01a.bundle -s ipn:1.1 --keys $KEYS --kid "aesgcmkey_32" --target payload` | `bundle_jq ./test_data/enc_01a.bundle 'any(.extension_blocks[]; .block_type == 12 and .security_target[0] == 1)'` |
| **ENC-01b** | **Encrypt Payload**<br><br>Apply AES-GCM encryption to the payload. | `$BUNDLE encrypt ./test_data/create_01.bundle -o ./test_data/enc_01b.bundle -s ipn:1.1 --keys $KEYS --kid "aesgcmkey_32" --block-id 1` | `bundle_jq ./test_data/enc_01b.bundle 'any(.extension_blocks[]; .block_type == 12 and .security_target[0] == 1)'` |
| **ENC-02** | **Decrypt Payload**<br><br>Decrypt a valid BCB using the correct key. | `$BUNDLE decrypt ./test_data/enc_01a.bundle -o ./test_data/dec_02.bundle --keys $KEYS` | Decrypts ciphertext in memory.<br><br>Outputs original plaintext data.<br><br>Validates GCM Auth Tag. |
| **ENC-03** | **Bad Key Decryption**<br><br>Attempt decryption with incorrect key material. | `$BUNDLE decrypt ./test_data/enc_01a.bundle -o ./test_data/dec_03.bundle --keys ./test_data/wrong_keys.json` | Returns failure.<br><br>Auth Tag validation fails. |
| **ENC-04** | **Alternate Ciphersuite**<br><br>Apply AES-128-GCM encryption. (LLR 2.2.5) | `$BUNDLE encrypt ./test_data/create_01.bundle -o ./test_data/enc_04.bundle -s ipn:1.1 --keys $KEYS --kid "aesgcmkey_16" --target payload` | `bundle_jq ./test_data/enc_04.bundle 'any(.extension_blocks[]; .block_type == 12 and .cipher_suite_id == 1)'` |
| **ENC-05** | **AES Key Wrap**<br><br>Encrypt payload using a session key wrapped with a KEK. (LLR 2.2.7) | `$BUNDLE encrypt ./test_data/create_01.bundle -o ./test_data/enc_05.bundle -s ipn:1.1 --keys $KEYS --kid "aesgcmkey_32_kw" --target payload` | `bundle_jq ./test_data/enc_05.bundle 'any(.extension_blocks[]; .block_type == 12 and .security_target[0] == 1)'` |
| **ENC-06** | **Encrypt Signed Payload**<br><br>Encrypt a bundle with a signed payload; verify BIB is also encrypted. | `$BUNDLE encrypt ./test_data/sign_01a.bundle -o ./test_data/enc_06.bundle -s ipn:1.1 --keys $KEYS --kid "aesgcmkey_32" --target payload` | `bundle_jq ./test_data/enc_06.bundle 'any(.extension_blocks[]; .block_type == 12 and (.security_target | contains([1])) and (.security_target | length > 1))'` |

### Suite 5: Validation & Inspection (LLR 1.1.1, 1.1.15, 1.1.16, 1.1.17, 1.1.21, 1.1.33)

*Objective: Verify the Parser and Validator logic.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **VALID-01** | **Validate Compliant Bundle**<br><br>Run all internal consistency checks on a valid bundle. (LLR 1.1.15, 1.1.16, 1.1.17, 1.1.21, 1.1.27, 1.1.28) | `$BUNDLE validate ./test_data/create_01.bundle` | Returns success/valid status.<br><br>Checks: CRC validity, Block ordering, EID format compliance. |
| **VALID-02** | **Validate Invalid CRC**<br><br>Verify detection of a corrupt CRC. (LLR 1.1.21) | `[External Mod]`<br><br>`$BUNDLE validate ./test_data/bad_crc.bundle` | Returns failure with a specific CRC error. |
| **VALID-03** | **Validate Missing Age**<br><br>Verify rejection of a bundle with timestamp 0 but no Bundle Age block. (LLR 1.1.33) | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 3600 --creation-time 0 -o ./test_data/valid_03.bundle`<br><br>`! $BUNDLE validate ./test_data/valid_03.bundle` | Command returns failure with a specific error about the missing Bundle Age block. |
| **INSP-01** | **Print Structure**<br><br>Output textual representation of the bundle. | `$BUNDLE print ./test_data/create_01.bundle` | correctly parses all blocks.<br><br>Displays fields (Source, Dest, Block Types) accurately. |
| **INSP-03** | **Inspect Encrypted**<br><br>Print an encrypted bundle *without* keys. | `$BUNDLE print ./test_data/enc_01a.bundle` | Shows Payload Block as "Encrypted" or "Ciphertext".<br><br>Does NOT leak plaintext. |

### Suite 6: Rewriting & Canonicalization (LLR 1.1.30, 1.1.31)

*Objective: Verify bundle rewriting logic for robustness and canonicalization.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **REWRITE-01** | **Reorder Blocks**<br><br>Canonicalize a bundle with blocks in the wrong order. (LLR 1.1.31) | `$BUNDLE rewrite ./test_data/non_canonical.bundle -o ./test_data/rewrite_01.bundle`<br><br>`$BUNDLE validate ./test_data/rewrite_01.bundle` | Returns success.<br><br>The `validate` command passes, indicating the block order is now canonical. |
| **REWRITE-02** | **Discard Unknown Block**<br><br>Discard an unrecognized, non-critical extension block. (LLR 1.1.30) | `$BUNDLE rewrite ./test_data/unknown_block.bundle -o ./test_data/rewrite_02.bundle` | `bundle_jq ./test_data/rewrite_02.bundle '(.extension_blocks | length) == 0'`<br><br>The rewritten bundle contains no extension blocks. |
