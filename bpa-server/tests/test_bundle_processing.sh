#!/bin/bash
# Test script to process a bundle through the full BPA server
#
# Usage:
#   ./bpa-server/tests/test_bundle_processing.sh [-o output_dir] [-n node_id] [-i] [bundle_file]
#
# Options:
#   -o output_dir   Save output bundles (e.g., status reports) to this directory
#   -n node_id      Set the BPA node ID (default: ipn:1.0)
#   -i              Interactive mode: start the server and wait for manual input
#
# By default a test bundle destined for the file-cla peer ipn:2.0 is
# generated with the `bundle` tool and the script asserts it traverses the
# BPA (consumed from the outbox, written to the peer inbox), exiting
# nonzero on failure. Pass a bundle_file to process that bundle instead.
# In interactive mode the server watches the outbox directory - copy
# bundles there to process them.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Parse options
OUTPUT_DIR=""
NODE_ID="ipn:1.0"
INTERACTIVE=false
while getopts "o:n:i" opt; do
    case $opt in
        o)
            OUTPUT_DIR="$OPTARG"
            ;;
        n)
            NODE_ID="$OPTARG"
            ;;
        i)
            INTERACTIVE=true
            ;;
        \?)
            echo "Invalid option: -$OPTARG" >&2
            exit 1
            ;;
    esac
done
shift $((OPTIND-1))

# Create output directory if specified
if [ -n "$OUTPUT_DIR" ]; then
    mkdir -p "$OUTPUT_DIR"
    OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"
    echo "Output bundles will be saved to: $OUTPUT_DIR"
fi

# Create temporary directory for test
TEST_DIR=$(mktemp -d)
echo "Using test directory: $TEST_DIR"

# Cleanup on exit
cleanup() {
    echo "Cleaning up..."
    if [ -n "$BPA_PID" ] && kill -0 "$BPA_PID" 2>/dev/null; then
        kill "$BPA_PID" 2>/dev/null || true
        wait "$BPA_PID" 2>/dev/null || true
    fi

    # Capture output bundles before cleanup if output dir specified
    if [ -n "$OUTPUT_DIR" ] && [ -d "$TEST_DIR/inbox" ]; then
        OUTPUT_COUNT=$(find "$TEST_DIR/inbox" -type f 2>/dev/null | wc -l)
        if [ "$OUTPUT_COUNT" -gt 0 ]; then
            echo "Saving $OUTPUT_COUNT output bundle(s) to $OUTPUT_DIR"
            cp -v "$TEST_DIR/inbox"/* "$OUTPUT_DIR/" 2>/dev/null || true
        fi
    fi

    rm -rf "$TEST_DIR"
}
trap cleanup EXIT

# Poll until a file contains at least N occurrences of a pattern, with a
# deadline in seconds. Fails fast when the given process dies first.
wait_for_log() {
    local file=$1 pattern=$2 count=$3 deadline=$4 pid=${5:-}
    local waited=0 seen
    while :; do
        seen=$(grep -c "$pattern" "$file" 2>/dev/null) || true
        if [ "${seen:-0}" -ge "$count" ]; then
            return 0
        fi
        if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
            echo "ERROR: process $pid exited while waiting for '$pattern'"
            return 1
        fi
        if [ "$waited" -ge $((deadline * 10)) ]; then
            echo "ERROR: timed out waiting for ${count}x '$pattern' in $file"
            return 1
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
}

# Poll until a shell condition holds, with a deadline in seconds.
wait_for() {
    local deadline=$1
    shift
    local waited=0
    while ! "$@"; do
        if [ "$waited" -ge $((deadline * 10)) ]; then
            return 1
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
    return 0
}

# Create directories
mkdir -p "$TEST_DIR/outbox"
mkdir -p "$TEST_DIR/inbox"
mkdir -p "$TEST_DIR/bundles"
mkdir -p "$TEST_DIR/metadata"

# Create static routes file - reflect anything that has no peer route
cat > "$TEST_DIR/static_routes" << 'EOF'
# Forward all bundles - reflect back to sender for testing
*:** reflect
EOF

# Create config file
echo "BPA Node ID: $NODE_ID"

cat > "$TEST_DIR/config.toml" << EOF
log-level = "debug"
status-reports = true
node-ids = "$NODE_ID"

[static-routes]
routes-file = "$TEST_DIR/static_routes"
watch = "none"

[storage.metadata]
type = "memory"

[storage.bundle]
type = "memory"

[[clas]]
name = "file-test"
type = "file-cla"
outbox = "$TEST_DIR/outbox"
[clas.peers]
"ipn:2.0" = "$TEST_DIR/inbox"
EOF

echo "=== Configuration ==="
cat "$TEST_DIR/config.toml"
echo ""
echo "=== Static Routes ==="
cat "$TEST_DIR/static_routes"
echo ""

# Build bpa-server with file-cla feature (and the bundle tool if we need
# to generate the test bundle). Set BUILD_PROFILE=debug for a faster
# local build.
BUILD_PROFILE="${BUILD_PROFILE:-release}"
if [ "$BUILD_PROFILE" = "release" ]; then
    CARGO_PROFILE_FLAG="--release"
else
    CARGO_PROFILE_FLAG=""
fi

echo "=== Building bpa-server with file-cla ($BUILD_PROFILE) ==="
cd "$WORKSPACE_DIR"
# A minimal build: the config below selects the memory backends
# explicitly, so no storage feature is needed.
cargo build $CARGO_PROFILE_FLAG -p hardy-bpa-server --no-default-features --features file-cla

BPA_BIN="$WORKSPACE_DIR/target/$BUILD_PROFILE/hardy-bpa-server"

if [ ! -x "$BPA_BIN" ]; then
    echo "ERROR: Failed to build hardy-bpa-server"
    exit 1
fi

# Determine bundle file to use
BUNDLE_FILE="${1:-}"
if [ -z "$BUNDLE_FILE" ] && [ "$INTERACTIVE" = false ]; then
    echo ""
    echo "=== Generating test bundle ==="
    cargo build $CARGO_PROFILE_FLAG -p hardy-bpv7-tools
    BUNDLE_BIN="$WORKSPACE_DIR/target/$BUILD_PROFILE/bundle"
    BUNDLE_FILE="$TEST_DIR/test.bundle"
    "$BUNDLE_BIN" create --source "ipn:3.1" --destination "ipn:2.0" \
        --payload "Hello, bundle processing test" --output "$BUNDLE_FILE"
fi

BPA_LOG="$TEST_DIR/bpa.log"

echo ""
echo "=== Starting BPA Server ==="
echo "Server log: $BPA_LOG"
"$BPA_BIN" -c "$TEST_DIR/config.toml" > "$BPA_LOG" 2>&1 &
BPA_PID=$!

# Wait for the server to report readiness
if ! wait_for_log "$BPA_LOG" "Started successfully" 1 20 "$BPA_PID"; then
    echo "ERROR: BPA server failed to start"
    cat "$BPA_LOG"
    exit 1
fi

echo "BPA server started with PID $BPA_PID"

if [ "$INTERACTIVE" = true ]; then
    echo ""
    echo "Interactive mode."
    echo "To test, copy a bundle file to: $TEST_DIR/outbox/"
    echo ""
    echo "Example:"
    echo "  cp your_bundle.bundle $TEST_DIR/outbox/"
    echo ""
    if [ -n "$OUTPUT_DIR" ]; then
        echo "Output bundles will be saved to: $OUTPUT_DIR"
        echo ""
    fi
    echo "Press Ctrl+C to stop the server..."
    wait "$BPA_PID"
    exit 0
fi

if [ ! -f "$BUNDLE_FILE" ]; then
    echo "ERROR: Bundle file not found: $BUNDLE_FILE"
    exit 1
fi

echo ""
echo "=== Submitting bundle to BPA ==="
echo "Bundle: $BUNDLE_FILE"
cp "$BUNDLE_FILE" "$TEST_DIR/outbox/test_bundle.bin"

FAILED=0

# The bundle must be consumed from the outbox
echo "Waiting for bundle to be processed..."
if wait_for 20 test ! -f "$TEST_DIR/outbox/test_bundle.bin"; then
    echo "Bundle file was consumed from outbox"
else
    echo "ERROR: Bundle file still in outbox - not processed"
    FAILED=1
fi

# ... and an output bundle must appear in the peer inbox
inbox_has_output() {
    [ "$(find "$TEST_DIR/inbox" -type f 2>/dev/null | wc -l)" -gt 0 ]
}
if wait_for 20 inbox_has_output; then
    OUTPUT_COUNT=$(find "$TEST_DIR/inbox" -type f 2>/dev/null | wc -l)
    echo ""
    echo "=== Output Bundles ($OUTPUT_COUNT) ==="
    ls -la "$TEST_DIR/inbox"

    if [ -n "$OUTPUT_DIR" ]; then
        echo ""
        echo "Output bundles will be saved to: $OUTPUT_DIR"
    fi
else
    echo "ERROR: no output bundle appeared in the peer inbox"
    FAILED=1
fi

echo ""
if [ "$FAILED" -ne 0 ]; then
    echo "=== Test FAILED ==="
    echo "Last 50 lines of the server log:"
    tail -n 50 "$BPA_LOG"
    exit 1
fi

echo "=== Test Complete ==="
echo ""
echo "Stopping BPA server..."
