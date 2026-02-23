#!/bin/bash
# Start DTNME in Docker for interactive testing
#
# Usage:
#   ./tests/interop/DTNME/start_dtnme.sh
#
# Then in another terminal:
#   bp ping ipn:1.7 127.0.0.1:4556 --source ipn:2.12345 --listen [::]:4557 --wait 10s --no-sign --no-crc

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

DTNME_IMAGE="dtnme-interop"
DTNME_NODE_NUM=1
DTNME_PORT=4556
HARDY_NODE_NUM=2
HARDY_PORT=4557

# Cleanup function for signal handling
cleanup() {
    echo ""
    echo "Stopping DTNME container..."
    docker stop -t 2 dtnme-interop-test 2>/dev/null || true
    docker rm -f dtnme-interop-test 2>/dev/null || true
    echo "Cleanup complete"
    exit 0
}

# Trap INT (Ctrl+C), TERM, and EXIT signals
trap cleanup INT TERM

# Build image if needed
if ! docker image inspect "$DTNME_IMAGE" &>/dev/null; then
    echo "Building dtnme-interop image (this may take a while)..."
    docker build -t "$DTNME_IMAGE" "$SCRIPT_DIR/docker"
fi

# Stop any existing container
docker rm -f dtnme-interop-test 2>/dev/null || true

echo "Starting DTNME (ipn:$DTNME_NODE_NUM.0) with TCP CL on port $DTNME_PORT..."
docker run --rm \
    --name dtnme-interop-test \
    --network host \
    -e NODE_ID="$DTNME_NODE_NUM" \
    -e TCPCL_PORT="$DTNME_PORT" \
    -e REMOTE_HOST="127.0.0.1" \
    -e REMOTE_PORT="$HARDY_PORT" \
    -e REMOTE_NODE="$HARDY_NODE_NUM" \
    "$DTNME_IMAGE" &

CONTAINER_PID=$!
sleep 5

# Start echo service in the container
echo "Starting echo service on ipn:$DTNME_NODE_NUM.7..."
docker exec -d dtnme-interop-test /dtn/bin/echo_me -B 5010 -s "ipn:$DTNME_NODE_NUM.7"

sleep 1

echo ""
echo "============================================"
echo "DTNME ready for testing"
echo ""
echo "  Node:     ipn:$DTNME_NODE_NUM.0"
echo "  Echo:     ipn:$DTNME_NODE_NUM.7"
echo "  TCP CL:   127.0.0.1:$DTNME_PORT"
echo ""
echo "Test with:"
echo "  bp ping ipn:$DTNME_NODE_NUM.7 127.0.0.1:$DTNME_PORT \\"
echo "    --source ipn:$HARDY_NODE_NUM.12345 \\"
echo "    --listen [::]:$HARDY_PORT \\"
echo "    --wait 10s \\"
echo "    --no-sign --no-crc"
echo ""
echo "Press Ctrl+C to stop..."
echo "============================================"

# Wait for container (cleanup will be called on Ctrl+C via trap)
docker wait dtnme-interop-test || true

# If container exited on its own, clean up
cleanup
