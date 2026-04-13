#!/bin/bash
#
# Generate fuzz coverage reports for all bpv7 fuzz targets.
#
# Usage: ./coverage.sh
#
# Prerequisites:
#   - cargo +nightly fuzz
#   - Corpus directories populated (run fuzzers first)
#

set -e

TARGETS="random_bundles eid_cbor eid_str"
PROFILE_DIR="./fuzz/coverage"
TARGET_TRIPLE="x86_64-unknown-linux-gnu"
BIN_DIR="./target/${TARGET_TRIPLE}/coverage/${TARGET_TRIPLE}/release"

echo "=== Generating fuzz coverage for bpv7 ==="
echo

for target in $TARGETS; do
    echo "--- ${target} ---"

    # 1. Replay corpus against instrumented build
    echo "  Replaying corpus..."
    cargo +nightly fuzz coverage "$target"

    # 2. Export lcov
    echo "  Exporting lcov..."
    cargo +nightly cov -- export --format=lcov \
        -instr-profile "${PROFILE_DIR}/${target}/coverage.profdata" \
        "${BIN_DIR}/${target}" \
        -ignore-filename-regex='/.cargo/|rustc/|/target/' \
        > "${PROFILE_DIR}/${target}/lcov.info"

    # 3. Summary
    echo "  Summary:"
    lcov --summary "${PROFILE_DIR}/${target}/lcov.info" 2>&1 | grep -E 'lines|functions|branches'

    # 4. HTML
    echo "  Generating HTML..."
    cargo +nightly cov -- show --format=html \
        -instr-profile "${PROFILE_DIR}/${target}/coverage.profdata" \
        "${BIN_DIR}/${target}" \
        -o "${PROFILE_DIR}/${target}/" \
        -ignore-filename-regex='/.cargo/|rustc/|/target/'

    echo "  HTML report: ${PROFILE_DIR}/${target}/index.html"
    echo
done

echo "=== Done ==="
