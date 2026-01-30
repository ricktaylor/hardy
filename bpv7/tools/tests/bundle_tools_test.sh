#!/bin/bash
#
# Bundle Tools Integration Test Suite
#
# This script tests the hardy-bpv7-tools bundle command and its subcommands.
# It exercises bundle creation, inspection, signing, encryption, block
# manipulation, and pipeline operations.
#
# Usage: ./bundle_tools_test.sh [--keep-output]
#
# Prerequisites:
#   - cargo (Rust toolchain)
#   - jq (JSON processor)
#   - openssl or /dev/urandom (for key generation)
#

set -e  # Exit on error

# Get the directory where this script is located
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Configuration
BUNDLE="cargo run --quiet --package hardy-bpv7-tools --bin bundle --"
CBOR="cargo run --quiet --package hardy-cbor-tools --bin cbor --"
OUT_DIR="${SCRIPT_DIR}/output"

# Create output directory for test artifacts
mkdir -p "${OUT_DIR}"

# Keys will be generated in the output directory
KEYS="${OUT_DIR}/test-keys.json"

# Generate base64url-encoded random bytes (no padding)
# Usage: random_base64url <num_bytes>
random_base64url() {
    local num_bytes=$1
    if command -v openssl &> /dev/null; then
        openssl rand -base64 "$num_bytes" | tr '+/' '-_' | tr -d '='
    else
        head -c "$num_bytes" /dev/urandom | base64 | tr '+/' '-_' | tr -d '='
    fi
}

# Generate random test keys (JWKS format)
generate_test_keys() {
    local hmac_key=$(random_base64url 32)  # 256-bit HMAC key
    local aes_key=$(random_base64url 32)   # 256-bit AES key

    cat > "${KEYS}" <<EOF
{
    "keys": [
        {
            "kty": "oct",
            "k": "${aes_key}",
            "key_ops": ["encrypt", "decrypt"],
            "enc": "A256GCM",
            "kid": "aesgcmkey_32"
        },
        {
            "kty": "oct",
            "k": "${hmac_key}",
            "key_ops": ["sign", "verify"],
            "alg": "HS256",
            "kid": "hmackey"
        }
    ]
}
EOF
    echo "   Generated random test keys: ${KEYS}"
}

# Generate fresh keys for this test run
echo "Generating random test keys..."
generate_test_keys

# Helper function to inspect bundle and query with jq
# Usage: bundle_jq <bundle_file> <jq_query>
bundle_jq() {
    if ! command -v jq &> /dev/null; then
        echo "ERROR: jq is required for this test suite" >&2
        exit 1
    fi
    ${BUNDLE} inspect --format json "$1" | jq -r "$2"
}

# Helper function to inspect bundle with keys and query with jq
bundle_jq_keys() {
    ${BUNDLE} inspect --keys "${KEYS}" --format json "$1" | jq -r "$2"
}

# Helper function to assert a condition
# Usage: assert_eq <actual> <expected> <message>
assert_eq() {
    if [ "$1" != "$2" ]; then
        echo "   FAIL: $3" >&2
        echo "      Expected: $2" >&2
        echo "      Got: $1" >&2
        exit 1
    fi
}

# Cleanup function
cleanup() {
    rm -rf "${OUT_DIR}"
}

# Parse arguments
KEEP_OUTPUT=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --keep-output)
            KEEP_OUTPUT=true
            shift
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Set up cleanup trap unless --keep-output is specified
if [ "$KEEP_OUTPUT" = false ]; then
    trap cleanup EXIT
fi

echo "=== Bundle Tools Integration Test Suite ==="
echo
echo "Keys file: ${KEYS}"
echo "Output dir: ${OUT_DIR}"
echo

# Check for jq
if ! command -v jq &> /dev/null; then
    echo "ERROR: jq is required for this test suite"
    echo "Please install jq: https://stedolan.github.io/jq/"
    exit 1
fi

# ============================================================================
echo "=== Part 1: Basic Bundle Operations ==="
echo

echo "1. Creating initial bundle with payload..."
echo -n "Ready to generate a 32-byte payload" > "${OUT_DIR}/expected_payload.txt"
${BUNDLE} create -s ipn:2.1 -d ipn:1.2 -r ipn:2.1 -l 16m40s -c 16 -o "${OUT_DIR}/test.bundle" --payload-file "${OUT_DIR}/expected_payload.txt"
SOURCE=$(bundle_jq "${OUT_DIR}/test.bundle" '.source')
DEST=$(bundle_jq "${OUT_DIR}/test.bundle" '.destination')
assert_eq "$SOURCE" "ipn:2.1" "source EID"
assert_eq "$DEST" "ipn:1.2" "destination EID"
echo "   PASS: Bundle created (source=$SOURCE, dest=$DEST)"

echo "2. Inspecting original bundle..."
${BUNDLE} inspect --keys "${KEYS}" -o "${OUT_DIR}/out_orig.md" "${OUT_DIR}/test.bundle"
echo "   PASS: Original bundle inspected and saved to out_orig.md"

echo "3. Extracting payload from original bundle..."
${BUNDLE} extract -o "${OUT_DIR}/payload_orig.txt" "${OUT_DIR}/test.bundle"
if cmp -s "${OUT_DIR}/payload_orig.txt" "${OUT_DIR}/expected_payload.txt"; then
    echo "   PASS: Payload extracted from original and verified"
else
    echo "   FAIL: Original payload mismatch!"
    exit 1
fi

# ============================================================================
echo
echo "=== Part 2: Block Manipulation (add-block, update-block, remove-block) ==="
echo

echo "4. Adding hop-count extension block..."
# Hop-count block data is CBOR array [limit, count] - use [30, 0] for limit=30, count=0
echo '[30, 0]' | ${CBOR} compose - | ${BUNDLE} add-block -t hop-count --payload-file - -o "${OUT_DIR}/test_with_hop.bundle" "${OUT_DIR}/test.bundle"
HOP_COUNT=$(bundle_jq "${OUT_DIR}/test_with_hop.bundle" '[.blocks[] | select(.type == "HopCount")] | length')
assert_eq "$HOP_COUNT" "1" "hop-count block present"
echo "   PASS: Hop-count block added"

echo "5. Adding bundle-age extension block..."
# Bundle-age block data is CBOR unsigned integer (milliseconds) - use 0 for fresh bundle
echo '0' | ${CBOR} compose - | ${BUNDLE} add-block -t age --payload-file - -o "${OUT_DIR}/test_with_blocks.bundle" "${OUT_DIR}/test_with_hop.bundle"
AGE_COUNT=$(bundle_jq "${OUT_DIR}/test_with_blocks.bundle" '[.blocks[] | select(.type == "BundleAge")] | length')
assert_eq "$AGE_COUNT" "1" "bundle-age block present"
echo "   PASS: Bundle-age block added"

echo "6. Removing hop-count block..."
HOP_BLOCK_NUM=$(bundle_jq "${OUT_DIR}/test_with_blocks.bundle" '.blocks | to_entries[] | select(.value.type == "HopCount") | .key')
${BUNDLE} remove-block -n "$HOP_BLOCK_NUM" -o "${OUT_DIR}/test_no_hop.bundle" "${OUT_DIR}/test_with_blocks.bundle"
HOP_COUNT=$(bundle_jq "${OUT_DIR}/test_no_hop.bundle" '[.blocks[] | select(.type == "HopCount")] | length')
assert_eq "$HOP_COUNT" "0" "hop-count block removed"
echo "   PASS: Hop-count block removed"

# ============================================================================
echo
echo "=== Part 3: Security Operations (sign, verify, encrypt, decrypt) ==="
echo

echo "7. Signing bundle (adding BIB)..."
${BUNDLE} sign -s ipn:2.1 --keys "${KEYS}" --kid "hmackey" -f all -o "${OUT_DIR}/test_signed.bundle" "${OUT_DIR}/test.bundle"
BIB_COUNT=$(bundle_jq "${OUT_DIR}/test_signed.bundle" '[.blocks[] | select(.type == "BlockIntegrity")] | length')
assert_eq "$BIB_COUNT" "1" "BIB block count after signing"
echo "   PASS: Bundle signed (BIB added)"

echo "8. Verifying signed bundle..."
${BUNDLE} verify --keys "${KEYS}" "${OUT_DIR}/test_signed.bundle"
echo "   PASS: Signature verified"

echo "9. Removing integrity protection from bundle..."
${BUNDLE} remove-integrity --keys "${KEYS}" -o "${OUT_DIR}/test_unsigned.bundle" "${OUT_DIR}/test_signed.bundle"
BIB_COUNT=$(bundle_jq "${OUT_DIR}/test_unsigned.bundle" '[.blocks[] | select(.type == "BlockIntegrity")] | length')
assert_eq "$BIB_COUNT" "0" "BIB block count after removal"
echo "   PASS: Integrity protection removed"

echo "10. Re-signing bundle..."
${BUNDLE} sign -s ipn:2.1 --keys "${KEYS}" --kid "hmackey" -f all -o "${OUT_DIR}/test_signed.bundle" "${OUT_DIR}/test_unsigned.bundle"
echo "   PASS: Bundle re-signed"

echo "11. Inspecting signed bundle..."
${BUNDLE} inspect --keys "${KEYS}" -o "${OUT_DIR}/out.md" "${OUT_DIR}/test_signed.bundle"
echo "   PASS: Signed bundle inspected and saved to out.md"

echo "12. Encrypting bundle (adding BCB)..."
${BUNDLE} encrypt -s ipn:2.1 --keys "${KEYS}" --kid "aesgcmkey_32" -o "${OUT_DIR}/test_enc.bundle" "${OUT_DIR}/test_signed.bundle"
# Per RFC9172: when encrypting a block protected by a BIB, the BIB must also be encrypted
# So we expect 2 BCB blocks: one for payload, one for BIB
BCB_COUNT=$(bundle_jq_keys "${OUT_DIR}/test_enc.bundle" '[.blocks[] | select(.type == "BlockSecurity")] | length')
BIB_COUNT=$(bundle_jq_keys "${OUT_DIR}/test_enc.bundle" '[.blocks[] | select(.type == "BlockIntegrity")] | length')
assert_eq "$BCB_COUNT" "2" "BCB block count after encryption (payload + BIB)"
assert_eq "$BIB_COUNT" "1" "BIB block count after encryption"
echo "   PASS: Bundle encrypted (2 BCB blocks added per RFC9172, BIB preserved)"

echo "13. Inspecting encrypted bundle..."
${BUNDLE} inspect --keys "${KEYS}" -o "${OUT_DIR}/out_enc.md" "${OUT_DIR}/test_enc.bundle"
echo "   PASS: Encrypted bundle inspected and saved to out_enc.md"

echo "14. Extracting payload from encrypted bundle..."
${BUNDLE} extract --keys "${KEYS}" -o "${OUT_DIR}/payload.txt" "${OUT_DIR}/test_enc.bundle"
if cmp -s "${OUT_DIR}/payload.txt" "${OUT_DIR}/expected_payload.txt"; then
    echo "   PASS: Payload extracted and verified (matches expected)"
else
    echo "   FAIL: Payload mismatch!"
    exit 1
fi

echo "15. Removing encryption from payload block..."
${BUNDLE} remove-encryption --keys "${KEYS}" -o "${OUT_DIR}/test_decrypted.bundle" "${OUT_DIR}/test_enc.bundle"
# Check that payload BCB has been removed but BIB's BCB remains
BCB_COUNT=$(bundle_jq_keys "${OUT_DIR}/test_decrypted.bundle" '[.blocks[] | select(.type == "BlockSecurity")] | length')
BIB_COUNT=$(bundle_jq_keys "${OUT_DIR}/test_decrypted.bundle" '[.blocks[] | select(.type == "BlockIntegrity")] | length')
assert_eq "$BCB_COUNT" "1" "BCB block count after payload decryption (BIB still encrypted)"
assert_eq "$BIB_COUNT" "1" "BIB block count after BCB removal"
echo "   PASS: Payload encryption removed (BIB still encrypted)"

echo "16. Verifying decrypted bundle still has BIB..."
${BUNDLE} verify --keys "${KEYS}" "${OUT_DIR}/test_decrypted.bundle"
echo "   PASS: BIB still intact after encryption removal"

echo "17. Removing encryption from BIB block..."
BIB_BLOCK_NUM=$(bundle_jq_keys "${OUT_DIR}/test_decrypted.bundle" '.blocks | to_entries[] | select(.value.type == "BlockIntegrity") | .key')
${BUNDLE} remove-encryption --keys "${KEYS}" --block "$BIB_BLOCK_NUM" -o "${OUT_DIR}/test_fully_decrypted.bundle" "${OUT_DIR}/test_decrypted.bundle"
# Check that all BCBs have been removed
BCB_COUNT=$(bundle_jq "${OUT_DIR}/test_fully_decrypted.bundle" '[.blocks[] | select(.type == "BlockSecurity")] | length')
BIB_COUNT=$(bundle_jq "${OUT_DIR}/test_fully_decrypted.bundle" '[.blocks[] | select(.type == "BlockIntegrity")] | length')
assert_eq "$BCB_COUNT" "0" "BCB block count after full decryption"
assert_eq "$BIB_COUNT" "1" "BIB block count (still present)"
echo "   PASS: BIB encryption removed (all encryption removed)"

# ============================================================================
echo
echo "=== Part 4: Block Operations on Encrypted Bundles (--keys support) ==="
echo

echo "18. Adding block to encrypted bundle (using --keys)..."
echo '[30, 0]' | ${CBOR} compose - | ${BUNDLE} add-block --keys "${KEYS}" -t hop-count --payload-file - -o "${OUT_DIR}/test_enc_with_hop.bundle" "${OUT_DIR}/test_enc.bundle"
HOP_COUNT=$(bundle_jq_keys "${OUT_DIR}/test_enc_with_hop.bundle" '[.blocks[] | select(.type == "HopCount")] | length')
assert_eq "$HOP_COUNT" "1" "hop-count block added to encrypted bundle"
echo "   PASS: Block added to encrypted bundle using --keys"

echo "19. Removing block from encrypted bundle (using --keys)..."
HOP_BLOCK_NUM=$(bundle_jq_keys "${OUT_DIR}/test_enc_with_hop.bundle" '.blocks | to_entries[] | select(.value.type == "HopCount") | .key')
${BUNDLE} remove-block --keys "${KEYS}" -n "$HOP_BLOCK_NUM" -o "${OUT_DIR}/test_enc_no_hop.bundle" "${OUT_DIR}/test_enc_with_hop.bundle"
HOP_COUNT=$(bundle_jq_keys "${OUT_DIR}/test_enc_no_hop.bundle" '[.blocks[] | select(.type == "HopCount")] | length')
assert_eq "$HOP_COUNT" "0" "hop-count block removed from encrypted bundle"
echo "   PASS: Block removed from encrypted bundle using --keys"

# ============================================================================
echo
echo "=== Part 5: Validation ==="
echo

echo "20. Validating all bundles..."
${BUNDLE} validate "${OUT_DIR}/test.bundle"
${BUNDLE} validate "${OUT_DIR}/test_signed.bundle"
${BUNDLE} validate --keys "${KEYS}" "${OUT_DIR}/test_enc.bundle"
${BUNDLE} validate --keys "${KEYS}" "${OUT_DIR}/test_decrypted.bundle"
${BUNDLE} validate "${OUT_DIR}/test_fully_decrypted.bundle"
echo "   PASS: All bundles valid"

echo "21. Validating final bundle structure..."
# Count total blocks: primary (0) + payload (1) + BIB (varies) = 3 blocks in fully decrypted bundle
BLOCK_COUNT=$(bundle_jq "${OUT_DIR}/test_fully_decrypted.bundle" '[.blocks[]] | length')
assert_eq "$BLOCK_COUNT" "3" "total block count (primary + payload + BIB)"
# Verify lifetime was preserved (16m40s = 1000 seconds)
LIFETIME_SECS=$(bundle_jq "${OUT_DIR}/test_fully_decrypted.bundle" '.lifetime.secs')
assert_eq "$LIFETIME_SECS" "1000" "bundle lifetime (16m40s = 1000 seconds)"
echo "   PASS: Final bundle structure validated"

# ============================================================================
echo
echo "=== Part 6: Rewrite and Piping ==="
echo

echo "22. Testing rewrite command..."
${BUNDLE} rewrite -o "${OUT_DIR}/test_rewritten.bundle" "${OUT_DIR}/test.bundle"
${BUNDLE} validate "${OUT_DIR}/test_rewritten.bundle"
echo "   PASS: Bundle rewritten successfully"

echo "23. Testing pipeline: create | sign | encrypt..."
echo "Pipeline test" | ${BUNDLE} create -s ipn:1.1 -d ipn:2.2 --payload-file - | \
    ${BUNDLE} sign --keys "${KEYS}" --kid hmackey - | \
    ${BUNDLE} encrypt --keys "${KEYS}" --kid aesgcmkey_32 -o "${OUT_DIR}/test_pipeline.bundle" -
${BUNDLE} validate --keys "${KEYS}" "${OUT_DIR}/test_pipeline.bundle"
BCB_COUNT=$(bundle_jq_keys "${OUT_DIR}/test_pipeline.bundle" '[.blocks[] | select(.type == "BlockSecurity")] | length')
BIB_COUNT=$(bundle_jq_keys "${OUT_DIR}/test_pipeline.bundle" '[.blocks[] | select(.type == "BlockIntegrity")] | length')
assert_eq "$BCB_COUNT" "2" "pipeline: BCB count"
assert_eq "$BIB_COUNT" "1" "pipeline: BIB count"
echo "   PASS: Pipeline test passed"

echo "24. Testing pipeline: remove-encryption | remove-integrity | extract..."
${BUNDLE} remove-encryption --keys "${KEYS}" "${OUT_DIR}/test_pipeline.bundle" | \
    ${BUNDLE} remove-encryption --keys "${KEYS}" -b 2 - | \
    ${BUNDLE} remove-integrity --keys "${KEYS}" - | \
    ${BUNDLE} extract -o "${OUT_DIR}/payload_pipeline.txt" -
if grep -q "Pipeline test" "${OUT_DIR}/payload_pipeline.txt"; then
    echo "   PASS: Full decryption pipeline test passed"
else
    echo "   FAIL: Pipeline payload mismatch!"
    exit 1
fi

echo "25. Signing primary block (block 0) with CRC..."
# RFC 9171 Section 4.3.1 allows both CRC and BIB on the primary block
${BUNDLE} create --source ipn:1.0 --destination ipn:4.23 --payload "primary block sign test" -o "${OUT_DIR}/test_primary.bundle"
${BUNDLE} sign --keys "${KEYS}" --kid hmackey -f all -b 0 -o "${OUT_DIR}/test_primary_signed.bundle" "${OUT_DIR}/test_primary.bundle"
BIB_COUNT=$(bundle_jq "${OUT_DIR}/test_primary_signed.bundle" '[.blocks[] | select(.type == "BlockIntegrity")] | length')
assert_eq "$BIB_COUNT" "1" "BIB block count after signing primary block"
# Verify the signature
${BUNDLE} verify --keys "${KEYS}" -b 0 "${OUT_DIR}/test_primary_signed.bundle"
echo "   PASS: Primary block signed and verified (CRC preserved per RFC 9171)"

echo "26. Rejecting bundle create with --crc-type none..."
# RFC 9171 Section 4.3.1 requires CRC on primary block (unless BIB present)
# bundle create should reject --crc-type none to prevent creating invalid bundles
if ${BUNDLE} create --source ipn:1.0 --destination ipn:2.0 --payload "test" --crc-type none -o "${OUT_DIR}/invalid.bundle" 2>/dev/null; then
    echo "   FAIL: bundle create should have rejected --crc-type none"
    exit 1
else
    echo "   PASS: bundle create correctly rejected --crc-type none"
fi

# ============================================================================
echo
echo "=== All 26 tests passed! ==="
echo

if [ "$KEEP_OUTPUT" = true ]; then
    echo "Generated files (kept with --keep-output):"
    ls -lh "${OUT_DIR}"/*.bundle "${OUT_DIR}"/*.md "${OUT_DIR}"/payload*.txt 2>/dev/null || true
fi
