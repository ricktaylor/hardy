#!/bin/bash
# Start HDTN in Docker for interactive testing
#
# Usage:
#   ./tests/interop/HDTN/start_hdtn.sh
#
# Then in another terminal:
#   bp ping ipn:10.2047 127.0.0.1:4556 --no-sign

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

HDTN_IMAGE="hdtn-interop"
HDTN_NODE_NUM=10
HDTN_PORT=4556

# Cleanup function for signal handling
cleanup() {
    echo ""
    echo "Stopping HDTN container..."
    docker stop -t 2 hdtn-interop-test 2>/dev/null || true
    docker rm -f hdtn-interop-test 2>/dev/null || true
    echo "Cleanup complete"
    exit 0
}

# Trap INT (Ctrl+C), TERM, and EXIT signals
trap cleanup INT TERM

# Build image if needed
if ! docker image inspect "$HDTN_IMAGE" &>/dev/null; then
    echo "Building hdtn-interop image (this may take a while)..."
    docker build -t "$HDTN_IMAGE" "$SCRIPT_DIR/docker"
fi

# Stop any existing container
docker rm -f hdtn-interop-test 2>/dev/null || true

echo "Starting HDTN (ipn:$HDTN_NODE_NUM.0) with TCPCLv4 on port $HDTN_PORT..."
docker run --rm \
    --name hdtn-interop-test \
    --network host \
    -e NODE_ID="$HDTN_NODE_NUM" \
    -e TCPCL_PORT="$HDTN_PORT" \
    "$HDTN_IMAGE" &

sleep 5

echo ""
echo "============================================"
echo "HDTN ready for testing"
echo ""
echo "  Node:     ipn:$HDTN_NODE_NUM.0"
echo "  Echo:     ipn:$HDTN_NODE_NUM.2047"
echo "  TCPCLv4:  127.0.0.1:$HDTN_PORT"
echo ""
echo "Test with:"
echo "  bp ping ipn:$HDTN_NODE_NUM.2047 127.0.0.1:$HDTN_PORT --no-sign"
echo ""
echo "Press Ctrl+C to stop..."
echo "============================================"

# Wait for container (cleanup will be called on Ctrl+C via trap)
docker wait hdtn-interop-test || true

# If container exited on its own, clean up
cleanup
