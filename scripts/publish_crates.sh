#!/bin/bash
# Publish all Hardy crates to crates.io in dependency order.
#
# Requires: `cargo login` already done.
#
# Usage:
#   ./scripts/publish_crates.sh              # Publish all (skips hardy-cbor, assumed already published)
#   ./scripts/publish_crates.sh --dry-run    # Dry run only
#   ./scripts/publish_crates.sh --from tier3 # Resume from a specific tier

set -e

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"

DRY_RUN=""
FROM_TIER=1

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN="--dry-run" ;;
        --from) shift; FROM_TIER="$1" ;;
        tier[0-9]) FROM_TIER="${arg#tier}" ;;
    esac
done

publish() {
    local crate="$1"
    echo "--- Publishing $crate ---"
    local output
    if output=$(cargo publish -p "$crate" $DRY_RUN 2>&1); then
        echo "  OK"
    elif echo "$output" | grep -q "already exists"; then
        echo "  Already published — skipping"
    else
        echo "$output"
        echo "  FAILED — aborting"
        exit 1
    fi
}

# Tier 1: No internal dependencies
if [ "$FROM_TIER" -le 1 ]; then
    echo "=== Tier 1: Leaf crates ==="
    publish hardy-cbor
    publish hardy-async
fi

# Tier 2: Depends on tier 1
if [ "$FROM_TIER" -le 2 ]; then
    echo "=== Tier 2: Depends on cbor/async ==="
    publish hardy-bpv7
fi

# Tier 3: Depends on tier 2
if [ "$FROM_TIER" -le 3 ]; then
    echo "=== Tier 3: Depends on bpv7 ==="
    publish hardy-eid-patterns
    publish hardy-otel
fi

# Tier 4: Depends on tier 3
if [ "$FROM_TIER" -le 4 ]; then
    echo "=== Tier 4: Depends on bpa ==="
    publish hardy-bpa
fi

# Tier 5: Depends on tier 4
if [ "$FROM_TIER" -le 5 ]; then
    echo "=== Tier 5: Leaf consumers ==="
    publish hardy-proto
    publish hardy-tcpclv4
    publish hardy-echo-service
    publish hardy-file-cla
    publish hardy-ipn-legacy-filter
    publish hardy-localdisk-storage
    publish hardy-sqlite-storage
    publish hardy-postgres-storage
    publish hardy-s3-storage
fi

echo ""
echo "=== Done ==="
if [ -n "$DRY_RUN" ]; then
    echo "(dry run — nothing was published)"
else
    echo "All crates published. Verify at https://crates.io/crates?q=hardy"
fi
