#!/bin/bash
#
# Generate fuzz coverage report for the BPA fuzz target.
#
# Usage: ./coverage.sh
#

set -e

TARGET="bpa"
PROFILE_DIR="./fuzz/coverage"
TARGET_TRIPLE="x86_64-unknown-linux-gnu"
BIN_DIR="./target/${TARGET_TRIPLE}/coverage/${TARGET_TRIPLE}/release"

echo "=== Generating fuzz coverage for bpa ==="

# 1. Replay corpus against instrumented build
echo "  Replaying corpus..."
cargo +nightly fuzz coverage "$TARGET"

# 2. Export lcov
echo "  Exporting lcov..."
cargo +nightly cov -- export --format=lcov \
    -instr-profile "${PROFILE_DIR}/${TARGET}/coverage.profdata" \
    "${BIN_DIR}/${TARGET}" \
    -ignore-filename-regex='/.cargo/|rustc/|/target/' \
    > "${PROFILE_DIR}/${TARGET}/lcov.info"

# 3. Summary
echo "  Summary:"
lcov --summary "${PROFILE_DIR}/${TARGET}/lcov.info" 2>&1 | grep -E 'lines|functions|branches'

# 4. HTML
echo "  Generating HTML..."
cargo +nightly cov -- show --format=html \
    -instr-profile "${PROFILE_DIR}/${TARGET}/coverage.profdata" \
    "${BIN_DIR}/${TARGET}" \
    -o "${PROFILE_DIR}/${TARGET}/" \
    -ignore-filename-regex='/.cargo/|rustc/|/target/'

echo "  HTML report: ${PROFILE_DIR}/${TARGET}/index.html"
echo "=== Done ==="
