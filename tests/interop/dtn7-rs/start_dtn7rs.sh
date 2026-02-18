#!/bin/bash
# Start dtn7-rs in Docker for interactive testing
#
# Usage:
#   ./tests/interop/dtn7-rs/start_dtn7rs.sh
#
# Then in another terminal:
#   bp ping ipn:23.7 127.0.0.1:4556 --no-sign

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

DTN7_IMAGE="dtn7-interop"
DTN7_NODE_NUM=23
DTN7_PORT=4556

# Cleanup function for signal handling
cleanup() {
    echo ""
    echo "Stopping dtn7-rs container..."
    docker stop -t 2 dtn7-interop-test 2>/dev/null || true
    docker rm -f dtn7-interop-test 2>/dev/null || true
    echo "Cleanup complete"
    exit 0
}

# Trap INT (Ctrl+C), TERM, and EXIT signals
trap cleanup INT TERM

# Build image if needed
if ! docker image inspect "$DTN7_IMAGE" &>/dev/null; then
    echo "Building dtn7-interop image..."
    # Use docker directory as context - Dockerfile clones from GitHub, doesn't need workspace files
    docker build -f "$SCRIPT_DIR/docker/Dockerfile.dtn7-rs" -t "$DTN7_IMAGE" "$SCRIPT_DIR/docker"
fi

# Stop any existing container
docker rm -f dtn7-interop-test 2>/dev/null || true

echo "Starting dtn7-rs (ipn:$DTN7_NODE_NUM.0) with TCPCLv4 on port $DTN7_PORT..."
docker run --rm \
    --name dtn7-interop-test \
    --network host \
    -e NODE_ID="$DTN7_NODE_NUM" \
    "$DTN7_IMAGE" \
    -d -i0 -r epidemic -C "tcp:port=$DTN7_PORT" &

sleep 3

echo "Starting dtnecho2 (listens on ipn:$DTN7_NODE_NUM.7)..."
docker exec -d dtn7-interop-test dtnecho2 -v

sleep 1

echo ""
echo "============================================"
echo "dtn7-rs ready for testing"
echo ""
echo "  Node:     ipn:$DTN7_NODE_NUM.0"
echo "  Echo:     ipn:$DTN7_NODE_NUM.7"
echo "  TCPCLv4:  127.0.0.1:$DTN7_PORT"
echo ""
echo "Test with:"
echo "  bp ping ipn:$DTN7_NODE_NUM.7 127.0.0.1:$DTN7_PORT --no-sign"
echo ""
echo "Press Ctrl+C to stop..."
echo "============================================"

# Wait for container (cleanup will be called on Ctrl+C via trap)
docker wait dtn7-interop-test || true

# If container exited on its own, clean up
cleanup
