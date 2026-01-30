# Component Test Plan: BPv7 Module (via CLI Driver)

| Document Info | Details |
 | ----- | ----- |
| **Functional Area** | Bundle Protocol Agent Core Logic |
| **Component** | `hardy-bpv7` Library |
| **Test Driver** | `hardy-bpv7-tools` (Binary: `bundle`), `hardy-cbor-tools` (Binary: `cbor`), `jq` |
| **Requirements Ref** | `DTN-LLR_v1.1` |
| **Standard Ref** | RFC 9171 (BPv7), RFC 9172 (BPSec), RFC 9173 (Contexts) |
| **Scope** | Verification of Library Parsing, Serialization, and Security Logic using the CLI harness. |
| **Test Suite ID** | COMP-BPV7-CLI-01 |

## 1. Introduction

This document details the component testing strategy for the `hardy-bpv7` library. We utilize the `hardy-bpv7-tools` CLI binary (`bundle`) as a "Test Driver" to stimulate the library's internal functions (Builder, Editor, Serializer, Validator).

The tests are organized into **Functional Suites**, where each suite targets a specific capability of the Bundle Protocol implementation.

**Prerequisites:**

* **Driver:** `bundle` binary (from `hardy-bpv7-tools`).

* **CBOR Tools:** `cbor` binary (from `hardy-cbor-tools`) for low-level CBOR manipulation.

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
export CBOR="cargo run --quiet --package hardy-cbor-tools --bin cbor --"
export KEYS="./test_data/test-keys.json"
export OUT="./test_data"

# Helper: Query bundle JSON with jq
function bundle_jq() {
  $BUNDLE inspect --format json "$1" | jq -e "$2"
}

# Helper: Query bundle JSON with keys
function bundle_jq_keys() {
  $BUNDLE inspect --keys $KEYS --format json "$1" | jq -e "$2"
}

export -f bundle_jq bundle_jq_keys
```

## 4. Functional Test Suites

### Suite 1: Bundle Creation (LLR 1.1.1, 1.1.25)

*Objective: Verify the Builder logic and Primary Block serialization.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **CREATE-01** | **Minimal Bundle**<br><br>Create a bundle with only mandatory fields (Source, Dest, Lifetime). | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload "test" -o $OUT/create_01.bundle` | `bundle_jq $OUT/create_01.bundle '.source == "ipn:1.1" and .destination == "ipn:2.1"'` |
| **CREATE-02** | **Payload from Stdin**<br><br>Stream data into the payload during creation. | `echo "StreamData" \| $BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload-file - -o $OUT/create_02.bundle` | `$BUNDLE extract $OUT/create_02.bundle \| grep -q "StreamData"` |
| **CREATE-03** | **Payload from File**<br><br>Load payload data from an existing file. | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload-file $OUT/payload.dat -o $OUT/create_03.bundle` | `$BUNDLE extract $OUT/create_03.bundle \| cmp - $OUT/payload.dat` |
| **CREATE-04** | **No CRC**<br><br>Create a bundle with no CRC. (LLR 1.1.29) | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload "test" --crc-type none -o $OUT/create_04.bundle` | `bundle_jq $OUT/create_04.bundle '.crc_type == "None"'` |
| **CREATE-05** | **CRC Type**<br><br>Create a bundle with a non-default CRC type. (LLR 1.1.22, 1.1.29) | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload "test" --crc-type crc16 -o $OUT/create_05.bundle` | `bundle_jq $OUT/create_05.bundle '.crc_type == "CRC16_X25"'` |
| **CREATE-06** | **Zero Timestamp**<br><br>Create a bundle with DTN Time 0 using low-level CBOR manipulation. (LLR 1.1.33) | See Note 1 below. | Validate that bundle with timestamp=0 and bundle-age block is accepted; without bundle-age it is rejected. |

**Note 1 (CREATE-06):** The `bundle create` command always uses the current timestamp. To test zero-timestamp bundles:
1. Create a bundle: `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload "test" -o $OUT/temp.bundle`
2. Convert to CDN: `$CBOR inspect $OUT/temp.bundle > $OUT/temp.cdn`
3. Edit CDN to set creation timestamp to `[0, 0]` (DTN time zero)
4. Reconstitute: `$CBOR compose $OUT/temp.cdn -o $OUT/create_06.bundle`
5. Add bundle-age block: `echo '5000' | $CBOR compose - | $BUNDLE add-block -t age --payload-file - $OUT/create_06.bundle -o $OUT/create_06_with_age.bundle`
6. Validate: `$BUNDLE validate $OUT/create_06_with_age.bundle` (should pass)
7. Negative test: `! $BUNDLE validate $OUT/create_06.bundle` (should fail - missing bundle-age)

### Suite 2: Extension Blocks (LLR 1.1.19, 1.1.34)

*Objective: Verify the Editor logic and handling of standard Extension Blocks.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **EXT-01** | **Add Hop Count (via create)**<br><br>Insert a Hop Count block using create flag. | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload "test" --hop-limit 5 -o $OUT/ext_01.bundle` | `bundle_jq $OUT/ext_01.bundle '.hop_count.limit == 5 and .hop_count.count == 0'` |
| **EXT-02** | **Add Hop Count (via add-block)**<br><br>Insert a Hop Count block using add-block. | `echo '[30, 0]' \| $CBOR compose - \| $BUNDLE add-block -t hop-count --payload-file - $OUT/create_01.bundle -o $OUT/ext_02.bundle` | `bundle_jq $OUT/ext_02.bundle '.hop_count.limit == 30'` |
| **EXT-03** | **Add Previous Node**<br><br>Insert a Previous Node EID block. | See Note 2 below. | `bundle_jq $OUT/ext_03.bundle '.previous_node == "ipn:9.9"'` |
| **EXT-04** | **Report-To EID**<br><br>Set the Report-To field in the Primary Block. | `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload "test" --report-to ipn:3.3 -o $OUT/ext_04.bundle` | `bundle_jq $OUT/ext_04.bundle '.report_to == "ipn:3.3"'` |
| **EXT-05** | **Bundle Age**<br><br>Insert a Bundle Age block. (LLR 1.1.33) | `echo '5000' \| $CBOR compose - \| $BUNDLE add-block -t age --payload-file - $OUT/create_01.bundle -o $OUT/ext_05.bundle` | `bundle_jq $OUT/ext_05.bundle '.age.secs == 5 and .age.nanos == 0'` |

**Note 2 (EXT-03):** Previous Node block requires CBOR-encoded EID. The EID `ipn:9.9` encodes as CBOR array `[2, [9, 9]]`:
```bash
echo '[2, [9, 9]]' | $CBOR compose - | $BUNDLE add-block -t prev --payload-file - $OUT/create_01.bundle -o $OUT/ext_03.bundle
```

### Suite 3: Integrity Operations (BIB) (LLR 2.1.1, 2.2.1, 2.2.3)

*Objective: Verify the Signer/Verifier logic and RFC 9172/9173 compliance.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **SIGN-01** | **Sign Payload (default)**<br><br>Apply HMAC-SHA256 to the payload block (block 1 is default). | `$BUNDLE sign --keys $KEYS --kid "hmackey" -s ipn:1.1 -o $OUT/sign_01.bundle $OUT/create_01.bundle` | `bundle_jq $OUT/sign_01.bundle '[.blocks[] \| select(.type == "BlockIntegrity")] \| length == 1'` |
| **SIGN-02** | **Sign Specific Block**<br><br>Apply integrity to a specific extension block (e.g., Hop Count at block 2). | `$BUNDLE sign --keys $KEYS --kid "hmackey" -s ipn:1.1 -b 2 -o $OUT/sign_02.bundle $OUT/ext_01.bundle` | BIB block present targeting block 2. |
| **SIGN-03** | **Verify Valid**<br><br>Verify a correctly signed bundle. | `$BUNDLE verify --keys $KEYS $OUT/sign_01.bundle` | Returns exit code 0 (success). |
| **SIGN-04** | **Verify Tampered**<br><br>Verify a bundle with modified payload bytes. | See Note 3 below. | Returns non-zero exit code with integrity mismatch error. |
| **SIGN-05** | **HMAC-SHA384**<br><br>Apply HMAC-SHA384 to the payload. (LLR 2.2.2) | `$BUNDLE sign --keys $KEYS --kid "hmackey_384" -s ipn:1.1 -o $OUT/sign_05.bundle $OUT/create_01.bundle` | BIB block present. Requires `hmackey_384` in keyfile. |
| **SIGN-06** | **HMAC-SHA512**<br><br>Apply HMAC-SHA512 to the payload. (LLR 2.2.3) | `$BUNDLE sign --keys $KEYS --kid "hmackey_512" -s ipn:1.1 -o $OUT/sign_06.bundle $OUT/create_01.bundle` | BIB block present. Requires `hmackey_512` in keyfile. |
| **SIGN-07** | **Remove Integrity**<br><br>Remove BIB from a signed bundle. | `$BUNDLE remove-integrity --keys $KEYS -o $OUT/sign_07.bundle $OUT/sign_01.bundle` | `bundle_jq $OUT/sign_07.bundle '[.blocks[] \| select(.type == "BlockIntegrity")] \| length == 0'` |

**Note 3 (SIGN-04):** To create a tampered bundle for verification testing:
1. Create and sign: `$BUNDLE sign --keys $KEYS --kid "hmackey" -o $OUT/signed.bundle $OUT/create_01.bundle`
2. Convert to hex, modify a payload byte, reconstitute
3. Verify: `! $BUNDLE verify --keys $KEYS $OUT/tampered.bundle` (should fail)

### Suite 4: Confidentiality Operations (BCB) (LLR 2.1.1, 2.2.6)

*Objective: Verify the Encryptor/Decryptor logic and AES-GCM implementation.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **ENC-01** | **Encrypt Payload (AES-256-GCM)**<br><br>Apply AES-GCM encryption to the payload block (default). | `$BUNDLE encrypt --keys $KEYS --kid "aesgcmkey_32" -s ipn:1.1 -o $OUT/enc_01.bundle $OUT/create_01.bundle` | `bundle_jq_keys $OUT/enc_01.bundle '[.blocks[] \| select(.type == "BlockSecurity")] \| length == 1'` |
| **ENC-02** | **Decrypt Payload**<br><br>Extract decrypted payload data from encrypted bundle. | `$BUNDLE decrypt --keys $KEYS -o $OUT/dec_02.txt $OUT/enc_01.bundle` | `cmp $OUT/dec_02.txt` with original payload ("test"). |
| **ENC-03** | **Bad Key Decryption**<br><br>Attempt decryption with incorrect key material. | `! $BUNDLE decrypt --keys $OUT/wrong_keys.json -o $OUT/dec_03.txt $OUT/enc_01.bundle` | Returns non-zero exit code. Auth Tag validation fails. |
| **ENC-04** | **AES-128-GCM**<br><br>Apply AES-128-GCM encryption. (LLR 2.2.5) | `$BUNDLE encrypt --keys $KEYS --kid "aesgcmkey_16" -s ipn:1.1 -o $OUT/enc_04.bundle $OUT/create_01.bundle` | BCB block present. Requires `aesgcmkey_16` (16-byte key) in keyfile. |
| **ENC-05** | **Encrypt Signed Payload**<br><br>Encrypt a bundle with a signed payload; verify BIB is also encrypted per RFC 9172. | `$BUNDLE encrypt --keys $KEYS --kid "aesgcmkey_32" -s ipn:1.1 -o $OUT/enc_05.bundle $OUT/sign_01.bundle` | `bundle_jq_keys $OUT/enc_05.bundle '[.blocks[] \| select(.type == "BlockSecurity")] \| length == 2'` (one BCB for payload, one for BIB). |
| **ENC-06** | **Remove Encryption (payload)**<br><br>Remove BCB from payload block. | `$BUNDLE remove-encryption --keys $KEYS -o $OUT/enc_06.bundle $OUT/enc_01.bundle` | `bundle_jq $OUT/enc_06.bundle '[.blocks[] \| select(.type == "BlockSecurity")] \| length == 0'` |
| **ENC-07** | **Remove Encryption (specific block)**<br><br>Remove BCB from a specific block in a multi-BCB bundle. | `$BUNDLE remove-encryption --keys $KEYS -b 2 -o $OUT/enc_07.bundle $OUT/enc_05.bundle` | One BCB removed, one remains. |

### Suite 5: Validation & Inspection (LLR 1.1.1, 1.1.15, 1.1.16, 1.1.17, 1.1.21, 1.1.33)

*Objective: Verify the Parser and Validator logic.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **VALID-01** | **Validate Compliant Bundle**<br><br>Run all internal consistency checks on a valid bundle. (LLR 1.1.15, 1.1.16, 1.1.17, 1.1.21, 1.1.27, 1.1.28) | `$BUNDLE validate $OUT/create_01.bundle` | Returns exit code 0 (success). Checks: CRC validity, Block ordering, EID format compliance. |
| **VALID-02** | **Validate Invalid CRC**<br><br>Verify detection of a corrupt CRC. (LLR 1.1.21) | See Note 4 below. | Returns non-zero exit code with CRC error message. |
| **VALID-03** | **Validate Missing Age**<br><br>Verify rejection of a bundle with timestamp 0 but no Bundle Age block. (LLR 1.1.33) | See CREATE-06 procedure, step 7. | Returns non-zero exit code with missing Bundle Age error. |
| **VALID-04** | **Validate Encrypted Bundle**<br><br>Validate an encrypted bundle with keys. | `$BUNDLE validate --keys $KEYS $OUT/enc_01.bundle` | Returns exit code 0 (success). |
| **INSP-01** | **Inspect (Markdown)**<br><br>Output human-readable representation of the bundle. | `$BUNDLE inspect $OUT/create_01.bundle` | Correctly parses and displays all blocks with Source, Dest, Block Types. |
| **INSP-02** | **Inspect (JSON)**<br><br>Output machine-readable JSON representation. | `$BUNDLE inspect --format json $OUT/create_01.bundle` | Valid JSON output parseable by jq. |
| **INSP-03** | **Inspect Encrypted (without keys)**<br><br>Inspect an encrypted bundle without providing keys. | `$BUNDLE inspect $OUT/enc_01.bundle` | Shows Payload Block as encrypted/ciphertext. Does NOT leak plaintext. |
| **INSP-04** | **Inspect Encrypted (with keys)**<br><br>Inspect an encrypted bundle with keys for decryption. | `$BUNDLE inspect --keys $KEYS $OUT/enc_01.bundle` | Shows decrypted payload content. |

**Note 4 (VALID-02):** To create a bundle with invalid CRC for testing:
1. Create valid bundle: `$BUNDLE create -s ipn:1.1 -d ipn:2.1 -l 1h --payload "test" -o $OUT/valid.bundle`
2. Convert to hex: `xxd $OUT/valid.bundle > $OUT/valid.hex`
3. Modify a CRC byte (last 4 bytes of primary block)
4. Reconstitute: `xxd -r $OUT/valid.hex > $OUT/bad_crc.bundle`
5. Validate: `! $BUNDLE validate $OUT/bad_crc.bundle` (should fail with CRC error)

### Suite 6: Rewriting & Canonicalization (LLR 1.1.30, 1.1.31)

*Objective: Verify bundle rewriting logic for robustness and canonicalization.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **REWRITE-01** | **Rewrite Valid Bundle**<br><br>Rewrite a valid bundle (should be unchanged). | `$BUNDLE rewrite -o $OUT/rewrite_01.bundle $OUT/create_01.bundle && $BUNDLE validate $OUT/rewrite_01.bundle` | Returns exit code 0 (success). Rewritten bundle is valid. |
| **REWRITE-02** | **Discard Unknown Block**<br><br>Discard an unrecognized, non-critical extension block. (LLR 1.1.30) | See Note 5 below. | The rewritten bundle contains no unrecognized extension blocks. |

**Note 5 (REWRITE-02):** To test discarding unknown blocks:
1. Create a bundle with an unknown block type using CBOR manipulation
2. Add an extension block with unrecognized type code (e.g., 200): Use `cbor compose` to create bundle with block type 200
3. Rewrite: `$BUNDLE rewrite $OUT/unknown_block.bundle -o $OUT/rewrite_02.bundle`
4. Verify the unknown block was discarded (if marked non-critical)

### Suite 7: Pipeline Operations

*Objective: Verify that commands can be chained via stdin/stdout.*

| Test ID | Scenario | Driver Command / Flags | Verification (jq) / Expected Behavior |
| --- | --- | --- | --- |
| **PIPE-01** | **Create → Sign → Encrypt**<br><br>Chain bundle creation with security operations. | `echo "test" \| $BUNDLE create -s ipn:1.1 -d ipn:2.2 --payload-file - \| $BUNDLE sign --keys $KEYS --kid hmackey - \| $BUNDLE encrypt --keys $KEYS --kid aesgcmkey_32 -o $OUT/pipe_01.bundle -` | `$BUNDLE validate --keys $KEYS $OUT/pipe_01.bundle` returns success. |
| **PIPE-02** | **Decrypt → Remove Integrity → Extract**<br><br>Chain decryption with payload extraction. | `$BUNDLE remove-encryption --keys $KEYS $OUT/pipe_01.bundle \| $BUNDLE remove-encryption --keys $KEYS -b 2 - \| $BUNDLE remove-integrity --keys $KEYS - \| $BUNDLE extract -o $OUT/pipe_02.txt -` | `grep -q "test" $OUT/pipe_02.txt` |
