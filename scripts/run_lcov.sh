#!/bin/bash
# Comprehensive lcov coverage collection for Hardy
#
# Runs unit tests and fuzz coverage for all instrumented crates,
# capturing results to lcov-results.txt for easy reference.
#
# Usage:
#   ./run_lcov.sh              # Run everything
#   ./run_lcov.sh --unit-only  # Skip fuzz coverage (faster)
#
# Output:
#   lcov-results.txt           # Summary of all coverage runs
#   lcov-*.info                # Individual lcov files
#   target/llvm-cov/html/      # HTML reports (unit tests)
#   */fuzz/coverage/*/          # Fuzz coverage data

set -e

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

RESULTS_FILE="lcov-results.txt"
UNIT_ONLY=false

if [ "$1" = "--unit-only" ]; then
    UNIT_ONLY=true
fi

echo "Hardy Coverage Report — $(date)" > "$RESULTS_FILE"
echo "==========================================" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# --- Unit test coverage ---

UNIT_CRATES=(
    hardy-cbor
    hardy-bpv7
    hardy-eid-patterns
    hardy-bpa
    hardy-proto
    hardy-tcpclv4
    hardy-otel
    hardy-async
    hardy-ipn-legacy-filter
    hardy-tvr
    hardy-sqlite-storage
    hardy-localdisk-storage
    hardy-bpa-server
    hardy-tcpclv4-server
)

echo "=== UNIT TEST COVERAGE ===" | tee -a "$RESULTS_FILE"
echo "" | tee -a "$RESULTS_FILE"

for crate in "${UNIT_CRATES[@]}"; do
    echo "--- $crate ---" | tee -a "$RESULTS_FILE"
    if cargo llvm-cov test --package "$crate" --lcov --output-path "lcov-${crate}.info" 2>&1; then
        lcov --summary "lcov-${crate}.info" 2>&1 | tee -a "$RESULTS_FILE"
    else
        echo "  FAILED — see output above" | tee -a "$RESULTS_FILE"
    fi
    echo "" | tee -a "$RESULTS_FILE"
done

if [ "$UNIT_ONLY" = true ]; then
    echo "Skipping fuzz coverage (--unit-only)" | tee -a "$RESULTS_FILE"
    echo ""
    echo "Results saved to $RESULTS_FILE"
    exit 0
fi

# --- Fuzz coverage ---
# Requires: cargo +nightly, corpus already populated
# Each target needs: fuzz coverage (generates profdata) + cov export (converts to lcov)

echo "=== FUZZ COVERAGE ===" | tee -a "$RESULTS_FILE"
echo "" | tee -a "$RESULTS_FILE"

FUZZ_TARGETS=(
    "cbor:decode"
    "bpv7:random_bundles"
    "bpv7:eid_cbor"
    "bpv7:eid_str"
    "eid-patterns:eid_pattern_str"
    "bpa:bpa"
    "tcpclv4:passive"
    "tcpclv4:active"
)

for entry in "${FUZZ_TARGETS[@]}"; do
    crate_dir="${entry%%:*}"
    target="${entry##*:}"
    echo "--- ${crate_dir}/fuzz: ${target} ---" | tee -a "$RESULTS_FILE"

    pushd "$crate_dir" > /dev/null

    if [ ! -d "fuzz/corpus/${target}" ] || [ -z "$(ls -A fuzz/corpus/${target} 2>/dev/null)" ]; then
        echo "  SKIPPED — no corpus (run fuzzer first)" | tee -a "../$RESULTS_FILE"
        popd > /dev/null
        echo "" | tee -a "$RESULTS_FILE"
        continue
    fi

    # Minimise corpus in-place before coverage run
    CORPUS_DIR="fuzz/corpus/${target}"
    BEFORE=$(ls "$CORPUS_DIR" | wc -l)
    echo "  Minimising corpus (${BEFORE} inputs)..." | tee -a "../$RESULTS_FILE"
    if cargo +nightly fuzz cmin "$target" 2>&1; then
        AFTER=$(ls "$CORPUS_DIR" | wc -l)
        echo "  Minimised to ${AFTER} inputs" | tee -a "../$RESULTS_FILE"
    else
        echo "  cmin failed — using original corpus" | tee -a "../$RESULTS_FILE"
    fi

    if cargo +nightly fuzz coverage "$target" 2>&1; then
        PROF_DATA="fuzz/coverage/${target}/coverage.profdata"
        # The instrumented binary is in target/<triple>/coverage/<triple>/release/<target>
        COV_BIN=$(find target -path "*/coverage/*/release/${target}" -type f -executable 2>/dev/null | head -1)

        if [ -n "$COV_BIN" ] && [ -f "$PROF_DATA" ]; then
            LCOV_RAW="../lcov-fuzz-${crate_dir}-${target}-raw.info"
            LCOV_OUT="../lcov-fuzz-${crate_dir}-${target}.info"
            SRC_DIR="$(pwd)/src/"
            if cargo +nightly cov -- export \
                --format=lcov \
                --instr-profile="$PROF_DATA" \
                "$COV_BIN" \
                > "$LCOV_RAW" 2>/dev/null; then
                # Filter to crate source files only (exclude dependencies)
                lcov --extract "$LCOV_RAW" "${SRC_DIR}*" -o "$LCOV_OUT" 2>/dev/null
                lcov --summary "$LCOV_OUT" 2>&1 | tee -a "../$RESULTS_FILE"
                rm -f "$LCOV_RAW"
            else
                echo "  lcov export failed" | tee -a "../$RESULTS_FILE"
            fi
        else
            echo "  Could not find profdata ($PROF_DATA) or binary (searched target/*/coverage/*/release/${target})" | tee -a "../$RESULTS_FILE"
        fi
    else
        echo "  FAILED — see output above" | tee -a "../$RESULTS_FILE"
    fi

    popd > /dev/null
    echo "" | tee -a "$RESULTS_FILE"
done

echo "==========================================" >> "$RESULTS_FILE"
echo "Completed: $(date)" >> "$RESULTS_FILE"

# Clean up lcov files — results are captured in $RESULTS_FILE
rm -f lcov-*.info

echo ""
echo "Results saved to $RESULTS_FILE"
