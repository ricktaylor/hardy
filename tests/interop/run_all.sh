#!/bin/bash
# Run All Interoperability Tests
#
# Runs the existing interop test scripts and extracts RTT statistics
# to produce a markdown comparison table.
#
# Usage:
#   ./tests/interop/run_all.sh [--skip-build] [--refresh] [--count N]
#
# Options:
#   --skip-build   Skip building Hardy binaries AND skip building any missing peer
#                  Docker image (such peers are reported "No image"). Default: build
#                  both, so a plain run will build absent peer images on demand.
#   --refresh      Force a clean rebuild (docker build --no-cache) of every peer
#                  image — picks up upstream/Dockerfile changes (passed to each script).
#   --count N      Pings per implementation (default 20; higher = tighter averages)
#
# Writes a generated results table to tests/interop/interop_results.md (git-tracked,
# do not edit by hand) and prints it to stdout.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Hardy build under test (git tag/commit), recorded in the results for provenance.
HARDY_VERSION="$(git -C "$WORKSPACE_DIR" describe --tags --always --dirty 2>/dev/null || echo unknown)"

# Configuration
PING_COUNT=20

# Parse options
SKIP_BUILD_FLAG=""
# Whether the *user* asked to skip building. When false (default), missing peer
# Docker images are built on demand; when true, peers without an image are skipped.
USER_SKIP_BUILD=false
# When set, force each peer image to be rebuilt (--no-cache); passed to peer scripts.
REFRESH_FLAG=""
while [[ $# -gt 0 ]]; do
    case $1 in
        --skip-build)
            SKIP_BUILD_FLAG="--skip-build"
            USER_SKIP_BUILD=true
            shift
            ;;
        --refresh)
            REFRESH_FLAG="--refresh"
            shift
            ;;
        --count|-c)
            PING_COUNT="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [--skip-build] [--count N]"
            exit 1
            ;;
    esac
done

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_step() { echo -e "${BLUE}[STEP]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }

# Results array (name|min|avg|max|stddev|loss|pings|avg_us)
declare -a RESULTS
BASELINE_AVG_US=""

# Convert duration string to microseconds for comparison
# Handles compound formats like: "42ms 450us", "12ms 273us 726ns", "1s 500ms", etc.
duration_to_us() {
    local dur="$1"
    local total_us=0

    # Extract and sum each component
    # Seconds
    if [[ "$dur" =~ ([0-9]+)s ]]; then
        total_us=$((total_us + ${BASH_REMATCH[1]} * 1000000))
    fi
    # Milliseconds
    if [[ "$dur" =~ ([0-9]+)ms ]]; then
        total_us=$((total_us + ${BASH_REMATCH[1]} * 1000))
    fi
    # Microseconds
    if [[ "$dur" =~ ([0-9]+)us ]]; then
        total_us=$((total_us + ${BASH_REMATCH[1]}))
    fi
    # Nanoseconds (convert to us, rounding)
    if [[ "$dur" =~ ([0-9]+)ns ]]; then
        total_us=$((total_us + (${BASH_REMATCH[1]} + 500) / 1000))
    fi

    if [ "$total_us" -gt 0 ]; then
        echo "$total_us"
    else
        echo ""
    fi
}

# Function to extract RTT stats from test output
# Looks for "rtt min/avg/max/stddev = X/Y/Z/W" lines
extract_rtt_stats() {
    local output="$1"
    local name="$2"

    # Find all RTT summary lines (there may be multiple tests)
    local rtt_lines=$(echo "$output" | grep "rtt min/avg/max/stddev" || true)

    if [ -z "$rtt_lines" ]; then
        RESULTS+=("$name|-|-|-|-|N/A|-|")
        return
    fi

    # Process each RTT line
    while IFS= read -r rtt_line; do
        if [ -n "$rtt_line" ]; then
            # Parse the values after the "="
            local values=$(echo "$rtt_line" | sed 's/.*= //')
            local min=$(echo "$values" | cut -d'/' -f1)
            local avg=$(echo "$values" | cut -d'/' -f2)
            local max=$(echo "$values" | cut -d'/' -f3)
            local stddev=$(echo "$values" | cut -d'/' -f4)

            # Extract loss from nearby line
            local loss=$(echo "$output" | grep -oP '\d+(\.\d+)?(?=% loss)' | head -1 || echo "?")

            # Extract ping counts from "N bundles transmitted, M received" line
            # Format: "30 bundles transmitted, 30 received, 0% loss" or "30 transmitted, 30 received"
            local stats_line=$(echo "$output" | grep -E '[0-9]+ (bundles )?transmitted' | head -1)
            local transmitted=""
            local received=""
            if [ -n "$stats_line" ]; then
                # Extract first number (transmitted) and number before "received"
                transmitted=$(echo "$stats_line" | sed -E 's/^([0-9]+).*/\1/')
                received=$(echo "$stats_line" | sed -E 's/.*,\s*([0-9]+)\s+received.*/\1/')
            fi
            [ -z "$transmitted" ] && transmitted="?"
            [ -z "$received" ] && received="?"
            local pings="${received}/${transmitted}"

            # Convert avg to microseconds for comparison
            local avg_us=$(duration_to_us "$avg")

            RESULTS+=("$name|$min|$avg|$max|$stddev|${loss}%|$pings|$avg_us")
        fi
    done <<< "$rtt_lines"
}

# =============================================================================
# Run tests
# =============================================================================
echo ""
echo "============================================================"
log_info "DTN Implementation Benchmark ($PING_COUNT pings each)"
echo "============================================================"
echo ""

# Hardy-to-Hardy (baseline)
# Run a simple single-direction test as baseline
log_step "Running Hardy baseline test..."

BP_BIN="$WORKSPACE_DIR/target/release/bp"
BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"

# Build if needed
if [ -z "$SKIP_BUILD_FLAG" ]; then
    log_info "Building Hardy..."
    if ! (cd "$WORKSPACE_DIR" && cargo build --release \
        -p hardy-tools \
        -p hardy-bpa-server); then
        log_warn "Build failed, skipping Hardy baseline"
        RESULTS+=("Hardy|-|-|-|-|Build failed|-|")
        BPA_BIN=""
        BP_BIN=""
    fi

    # Build the MTCP/STCP CLA client for ION interop
    MTCP_CLA_DIR="$SCRIPT_DIR/mtcp"
    if [ -d "$MTCP_CLA_DIR" ]; then
        log_info "Building MTCP/STCP CLA client..."
        (cd "$MTCP_CLA_DIR" && cargo build --release) || {
            log_warn "MTCP CLA client build failed, ION test may be skipped"
        }
    fi

    SKIP_BUILD_FLAG="--skip-build"
fi

if [ -x "$BPA_BIN" ] && [ -x "$BP_BIN" ]; then
    # Create temp config
    HARDY_TEST_DIR=$(mktemp -d)
    cat > "$HARDY_TEST_DIR/config.toml" << EOF
log-level = "warn"
status-reports = false
node-ids = "ipn:99.0"
[built-in-services]
echo = [7]
[storage.metadata]
type = "memory"
[storage.bundle]
type = "memory"
[[clas]]
name = "cl0"
type = "tcpclv4"
address = "[::]:4559"
must-use-tls = false
EOF

    # Start Hardy server
    "$BPA_BIN" -c "$HARDY_TEST_DIR/config.toml" &
    HARDY_PID=$!
    sleep 2

    if kill -0 "$HARDY_PID" 2>/dev/null; then
        # Run ping test
        OUTPUT=$("$BP_BIN" ping "ipn:99.7" "127.0.0.1:4559" \
            --source "ipn:1.1" \
            --count "$PING_COUNT" \
            --no-sign \
            2>&1) || true

        extract_rtt_stats "$OUTPUT" "Hardy"

        # Set baseline from first result (8 fields: name|min|avg|max|stddev|loss|pings|avg_us)
        if [ ${#RESULTS[@]} -gt 0 ]; then
            IFS='|' read -r _ _ _ _ _ _ _ avg_us <<< "${RESULTS[0]}"
            if [ -n "$avg_us" ]; then
                BASELINE_AVG_US="$avg_us"
                log_info "Baseline RTT: ${avg_us}us"
            fi
        fi

        # Cleanup
        kill "$HARDY_PID" 2>/dev/null || true
        wait "$HARDY_PID" 2>/dev/null || true
    else
        log_warn "Hardy server failed to start"
        RESULTS+=("Hardy|-|-|-|-|Failed to start|-|")
    fi

    rm -rf "$HARDY_TEST_DIR"
    log_info "Hardy baseline complete"
else
    log_warn "Hardy binaries not found, skipping baseline"
    RESULTS+=("Hardy|-|-|-|-|Not built|-|")
fi

# dtn7-rs
if [ -x "$SCRIPT_DIR/dtn7-rs/test_dtn7rs_ping.sh" ]; then
    if docker image inspect dtn7-interop &>/dev/null || [ "$USER_SKIP_BUILD" = false ]; then
        log_step "Running dtn7-rs test..."
        OUTPUT=$("$SCRIPT_DIR/dtn7-rs/test_dtn7rs_ping.sh" $SKIP_BUILD_FLAG $REFRESH_FLAG --count "$PING_COUNT" 2>&1) || true
        extract_rtt_stats "$OUTPUT" "dtn7-rs"
        log_info "dtn7-rs complete"
    else
        log_warn "dtn7-interop Docker image not found, skipping"
        RESULTS+=("dtn7-rs|-|-|-|-|No image|-|")
    fi
else
    log_warn "dtn7-rs test script not found, skipping"
fi

# HDTN
if [ -x "$SCRIPT_DIR/HDTN/test_hdtn_ping.sh" ]; then
    if docker image inspect hdtn-interop &>/dev/null || [ "$USER_SKIP_BUILD" = false ]; then
        log_step "Running HDTN test..."
        OUTPUT=$("$SCRIPT_DIR/HDTN/test_hdtn_ping.sh" $SKIP_BUILD_FLAG $REFRESH_FLAG --count "$PING_COUNT" 2>&1) || true
        extract_rtt_stats "$OUTPUT" "HDTN"
        log_info "HDTN complete"
    else
        log_warn "hdtn-interop Docker image not found, skipping"
        RESULTS+=("HDTN|-|-|-|-|No image|-|")
    fi
else
    log_warn "HDTN test script not found, skipping"
fi

# DTNME
if [ -x "$SCRIPT_DIR/DTNME/test_dtnme_ping.sh" ]; then
    if docker image inspect dtnme-interop &>/dev/null || [ "$USER_SKIP_BUILD" = false ]; then
        log_step "Running DTNME test..."
        OUTPUT=$("$SCRIPT_DIR/DTNME/test_dtnme_ping.sh" $SKIP_BUILD_FLAG $REFRESH_FLAG --count "$PING_COUNT" 2>&1) || true
        extract_rtt_stats "$OUTPUT" "DTNME"
        log_info "DTNME complete"
    else
        log_warn "dtnme-interop Docker image not found, skipping"
        RESULTS+=("DTNME|-|-|-|-|No image|-|")
    fi
else
    log_warn "DTNME test script not found, skipping"
fi

# ION
if [ -x "$SCRIPT_DIR/ION/test_ion_ping.sh" ]; then
    if docker image inspect ion-interop &>/dev/null || [ "$USER_SKIP_BUILD" = false ]; then
        log_step "Running ION test..."
        OUTPUT=$("$SCRIPT_DIR/ION/test_ion_ping.sh" $SKIP_BUILD_FLAG $REFRESH_FLAG --count "$PING_COUNT" 2>&1) || true
        extract_rtt_stats "$OUTPUT" "ION"
        log_info "ION complete"
    else
        log_warn "ion-interop Docker image not found, skipping"
        RESULTS+=("ION|-|-|-|-|No image|-|")
    fi
else
    log_warn "ION test script not found, skipping"
fi

# ud3tn (MTCP)
if [ -x "$SCRIPT_DIR/ud3tn/test_ud3tn_ping.sh" ]; then
    if docker image inspect ud3tn-interop &>/dev/null || [ "$USER_SKIP_BUILD" = false ]; then
        log_step "Running ud3tn test..."
        OUTPUT=$("$SCRIPT_DIR/ud3tn/test_ud3tn_ping.sh" $SKIP_BUILD_FLAG $REFRESH_FLAG --count "$PING_COUNT" 2>&1) || true
        extract_rtt_stats "$OUTPUT" "ud3tn"
        log_info "ud3tn complete"
    else
        log_warn "ud3tn-interop Docker image not found, skipping"
        RESULTS+=("ud3tn|-|-|-|-|No image|-|")
    fi
else
    log_warn "ud3tn test script not found, skipping"
fi

# NASA cFS (STCP)
if [ -x "$SCRIPT_DIR/NASA-cFS/test_cfs_ping.sh" ]; then
    if docker image inspect cfs-interop &>/dev/null || [ "$USER_SKIP_BUILD" = false ]; then
        log_step "Running NASA cFS test..."
        OUTPUT=$("$SCRIPT_DIR/NASA-cFS/test_cfs_ping.sh" $SKIP_BUILD_FLAG $REFRESH_FLAG --count "$PING_COUNT" 2>&1) || true
        extract_rtt_stats "$OUTPUT" "NASA cFS"
        log_info "NASA cFS complete"
    else
        log_warn "cfs-interop Docker image not found, skipping"
        RESULTS+=("NASA cFS|-|-|-|-|No image|-|")
    fi
else
    log_warn "NASA cFS test script not found, skipping"
fi

# ESA-BP (STCP)
if [ -x "$SCRIPT_DIR/ESA-BP/test_esa_bp_ping.sh" ]; then
    if docker image inspect esa-bp-interop &>/dev/null || [ "$USER_SKIP_BUILD" = false ]; then
        log_step "Running ESA-BP test..."
        OUTPUT=$("$SCRIPT_DIR/ESA-BP/test_esa_bp_ping.sh" $SKIP_BUILD_FLAG $REFRESH_FLAG --count "$PING_COUNT" 2>&1) || true
        extract_rtt_stats "$OUTPUT" "ESA-BP"
        log_info "ESA-BP complete"
    else
        log_warn "esa-bp-interop Docker image not found, skipping"
        RESULTS+=("ESA-BP|-|-|-|-|No image|-|")
    fi
else
    log_warn "ESA-BP test script not found, skipping"
fi

# =============================================================================
# Generate Markdown Table
# =============================================================================
echo ""
echo "============================================================"
log_info "RESULTS"
echo "============================================================"
echo ""

# Helper to calculate comparison percentage
calc_comparison() {
    local avg_us="$1"
    # Validate inputs are non-empty numeric values
    if [ -z "$avg_us" ] || [ -z "$BASELINE_AVG_US" ] || [ "$BASELINE_AVG_US" = "0" ]; then
        echo "-"
        return
    fi
    # Check that both values are numeric (digits only)
    if ! [[ "$avg_us" =~ ^[0-9]+$ ]] || ! [[ "$BASELINE_AVG_US" =~ ^[0-9]+$ ]]; then
        echo "-"
        return
    fi
    local pct=$(echo "scale=0; ($avg_us * 100) / $BASELINE_AVG_US" | bc 2>/dev/null)
    if [ -z "$pct" ] || ! [[ "$pct" =~ ^[0-9]+$ ]]; then
        echo "-"
        return
    fi
    if [ "$pct" -eq 100 ]; then
        echo "baseline"
    elif [ "$pct" -lt 100 ]; then
        echo "${pct}% (faster)"
    else
        echo "${pct}% (slower)"
    fi
}

# Print markdown table
echo "## DTN Implementation Ping Benchmark"
echo ""
echo "| Implementation | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |"
echo "|----------------|-----|-----|-----|--------|------|-------|----------|"

for result in "${RESULTS[@]}"; do
    IFS='|' read -r name min avg max stddev loss pings avg_us <<< "$result"
    comparison=$(calc_comparison "$avg_us")
    echo "| $name | $min | $avg | $max | $stddev | $loss | $pings | $comparison |"
done

echo ""
echo "_Benchmark: $PING_COUNT pings, $(date '+%Y-%m-%d %H:%M:%S')_"
echo ""

# Emit an "Implementation versions" table: the Hardy build plus, for each peer,
# the upstream git ref its Dockerfile pins (ARG *_REF) and the built image ID —
# so the results record exactly which implementations were tested against.
emit_versions() {
    echo "## Implementation versions"
    echo ""
    echo "| Implementation | Version | Image ID |"
    echo "|----------------|---------|----------|"
    echo "| Hardy | ${HARDY_VERSION} | built inline |"
    local meta name image dir df ref id esa_src desc pom
    for meta in \
        "dtn7-rs|dtn7-interop|dtn7-rs" \
        "HDTN|hdtn-interop|HDTN" \
        "DTNME|dtnme-interop|DTNME" \
        "ION|ion-interop|ION" \
        "ud3tn|ud3tn-interop|ud3tn" \
        "NASA cFS|cfs-interop|NASA-cFS" \
        "ESA-BP|esa-bp-interop|ESA-BP"; do
        IFS='|' read -r name image dir <<< "$meta"
        df="$SCRIPT_DIR/$dir/docker/Dockerfile"
        id=$(docker image inspect --format '{{.Id}}' "$image" 2>/dev/null | sed 's/sha256://; s/^\(.\{12\}\).*/\1/')
        id="${id:-not built}"
        if [ "$dir" = "ESA-BP" ]; then
            # ESA-BP builds from a local checkout (the ESCL export ships no .git): report
            # git describe of that checkout plus the Maven project version from src/pom.xml.
            esa_src="${ESA_BP_SRC:-$WORKSPACE_DIR/../esa-bp}"
            desc=$(git -C "$esa_src" describe --tags --always --dirty 2>/dev/null || true)
            pom=$(grep -m1 -oE '<version>[^<]+</version>' "$esa_src/src/pom.xml" 2>/dev/null | sed -E 's#</?version>##g')
            if [ -n "$desc" ] && [ -n "$pom" ]; then ref="$desc (declared: $pom)"
            elif [ -n "$desc" ]; then ref="$desc"
            else ref="${pom:-source build}"; fi
        else
            # Peers bake their version into /interop-version at build time (git describe +
            # declared manifest version); read it back, overriding the image entrypoint.
            ref=$(docker run --rm --entrypoint cat "$image" /interop-version 2>/dev/null | head -1)
            if [ -z "$ref" ]; then
                # Fallback: the upstream ref pinned in the Dockerfile (image not built yet).
                ref=$(grep -m1 -E '^ARG [A-Z0-9_]+_REF=' "$df" 2>/dev/null | sed -E 's/^ARG [A-Z0-9_]+_REF=//; s/[[:space:]].*//')
            fi
            ref="${ref:-not built}"
        fi
        echo "| $name | $ref | $id |"
    done
}

# Also save to file
OUTPUT_FILE="$SCRIPT_DIR/interop_results.md"
{
    echo "# DTN Implementation Ping Benchmark"
    echo ""
    echo "_Generated by \`tests/interop/run_all.sh\` — do not edit by hand._"
    echo ""
    echo "| Implementation | Min | Avg | Max | Stddev | Loss | Pings | vs Hardy |"
    echo "|----------------|-----|-----|-----|--------|------|-------|----------|"
    for result in "${RESULTS[@]}"; do
        IFS='|' read -r name min avg max stddev loss pings avg_us <<< "$result"
        comparison=$(calc_comparison "$avg_us")
        echo "| $name | $min | $avg | $max | $stddev | $loss | $pings | $comparison |"
    done
    echo ""
    emit_versions
    echo ""
    echo "## Notes"
    echo ""
    echo "- **Pings**: Received/Transmitted count"
    echo "- **vs Hardy**: Percentage relative to Hardy baseline (100% = same, >100% = slower)"
    echo "- Hardy, dtn7-rs, HDTN, DTNME use TCPCLv4; ud3tn uses MTCP; ION, ESA-BP, NASA cFS use STCP via client CLA"
    echo "- Hardy baseline runs inline; other tests use existing interop scripts"
    echo ""
    echo "_Hardy ${HARDY_VERSION}, $PING_COUNT pings per test, generated $(date '+%Y-%m-%d %H:%M:%S')_"
} > "$OUTPUT_FILE"

log_info "Results saved to: $OUTPUT_FILE"
