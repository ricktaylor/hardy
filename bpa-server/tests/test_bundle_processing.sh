#!/bin/bash
# Test script to process a bundle through the full BPA server
#
# Usage:
#   ./bpa-server/tests/test_bundle_processing.sh [-o output_dir] [-n node_id] [bundle_file]
#
# Options:
#   -o output_dir   Save output bundles (e.g., status reports) to this directory
#   -n node_id      Set the BPA node ID (default: ipn:1.0)
#
# If no bundle file is specified, starts the server and waits for manual input.
# The server watches an outbox directory - copy bundles there to process them.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Parse options
OUTPUT_DIR=""
NODE_ID="ipn:1.0"
while getopts "o:n:" opt; do
    case $opt in
        o)
            OUTPUT_DIR="$OPTARG"
            ;;
        n)
            NODE_ID="$OPTARG"
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

# Create directories
mkdir -p "$TEST_DIR/outbox"
mkdir -p "$TEST_DIR/inbox"
mkdir -p "$TEST_DIR/bundles"
mkdir -p "$TEST_DIR/metadata"

# Create static routes file - forward everything via file-cla or reflect
cat > "$TEST_DIR/static_routes" << 'EOF'
# Forward all bundles - reflect back to sender for testing
*:** reflect
EOF

# Create config file
echo "BPA Node ID: $NODE_ID"

cat > "$TEST_DIR/config.toml" << EOF
log_level = "debug"
status_reports = true
node_ids = "$NODE_ID"

[static_routes]
routes_file = "$TEST_DIR/static_routes"
watch = false

[metadata_storage]
type = "memory"

[bundle_storage]
type = "memory"

[[clas]]
name = "file-test"
type = "file-cla"
[clas.config]
outbox = "$TEST_DIR/outbox"
[clas.config.peers]
"ipn:2.0" = "$TEST_DIR/inbox"
EOF

echo "=== Configuration ==="
cat "$TEST_DIR/config.toml"
echo ""
echo "=== Static Routes ==="
cat "$TEST_DIR/static_routes"
echo ""

# Build bpa-server with file-cla feature
echo "=== Building bpa-server with file-cla ==="
cd "$WORKSPACE_DIR"
cargo build --release -p hardy-bpa-server --no-default-features --features file-cla

BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"

if [ ! -x "$BPA_BIN" ]; then
    echo "ERROR: Failed to build hardy-bpa-server"
    exit 1
fi

echo ""
echo "=== Starting BPA Server ==="
"$BPA_BIN" -c "$TEST_DIR/config.toml" &
BPA_PID=$!

# Wait for server to start
sleep 2

if ! kill -0 "$BPA_PID" 2>/dev/null; then
    echo "ERROR: BPA server failed to start"
    wait "$BPA_PID" || true
    exit 1
fi

echo "BPA server started with PID $BPA_PID"

# Determine bundle file to use
BUNDLE_FILE="${1:-}"
if [ -z "$BUNDLE_FILE" ]; then
    echo ""
    echo "No bundle file specified."
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

# Wait for processing
echo "Waiting for bundle to be processed..."
sleep 3

# Check if the bundle was processed (file should be removed from outbox)
if [ -f "$TEST_DIR/outbox/test_bundle.bin" ]; then
    echo "WARNING: Bundle file still in outbox - may not have been processed"
else
    echo "Bundle file was consumed from outbox"
fi

# Check for any output in inbox
OUTPUT_COUNT=$(find "$TEST_DIR/inbox" -type f 2>/dev/null | wc -l)
if [ "$OUTPUT_COUNT" -gt 0 ]; then
    echo ""
    echo "=== Output Bundles ($OUTPUT_COUNT) ==="
    ls -la "$TEST_DIR/inbox"

    if [ -n "$OUTPUT_DIR" ]; then
        echo ""
        echo "Output bundles will be saved to: $OUTPUT_DIR"
    fi
else
    echo "No output bundles generated"
fi

echo ""
echo "=== Test Complete ==="
echo "Check the log output above for any parsing errors."
echo ""
echo "Stopping BPA server..."
