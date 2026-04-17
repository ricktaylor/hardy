#!/bin/bash
# Runs each fuzz target in a continuous loop with corpus minimisation.
# Ctrl+C to stop. Corpus accumulates in fuzz/corpus/<target>/.
#
# Usage:
#   ./scripts/run_fuzz.sh                        # All targets, 60s each
#   ./scripts/run_fuzz.sh cbor:decode             # Single target
#   FUZZ_DURATION=300 ./scripts/run_fuzz.sh       # 5 minutes per target

set -e

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

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

DURATION="${FUZZ_DURATION:-60}"

# Filter to a single target if specified
if [ -n "$1" ]; then
    FUZZ_TARGETS=("$1")
fi

ROUND=1
while true; do
    echo ""
    echo "===== Round $ROUND ($(date)) ====="
    echo ""

    for entry in "${FUZZ_TARGETS[@]}"; do
        crate_dir="${entry%%:*}"
        target="${entry##*:}"

        echo "--- ${crate_dir}/${target} (${DURATION}s) ---"

        pushd "$crate_dir" > /dev/null
        cargo +nightly fuzz run "$target" -- \
            -max_total_time="$DURATION" \
            -max_len=65536 \
            2>&1 | tail -1
        BEFORE=$(ls fuzz/corpus/${target} 2>/dev/null | wc -l)

        # Minimise corpus in-place to focus subsequent runs
        if cargo +nightly fuzz cmin "$target" 2>&1 | tail -1; then
            AFTER=$(ls fuzz/corpus/${target} 2>/dev/null | wc -l)
            echo "  corpus: ${BEFORE} → ${AFTER} files"
        else
            echo "  corpus: ${BEFORE} files (cmin failed)"
        fi
        popd > /dev/null
        echo ""
    done

    ROUND=$((ROUND + 1))
done
