#!/bin/bash
# Start ION in Docker for interactive testing via STCP
#
# Usage:
#   ./tests/interop/ION/start_ion.sh
#
# Then in another terminal:
#   bp ping ipn:2.7 --cla /path/to/libhardy_mtcp_cla.so \
#       --cla-config '{"framing":"stcp","peer":"127.0.0.1:4556","peer-node":"ipn:2.0"}' \
#       --source ipn:1.12345 --no-sign

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

ION_IMAGE="ion-interop"
ION_NODE_NUM=2
ION_STCP_PORT=4556
HARDY_NODE_NUM=1
HARDY_STCP_PORT=4557

cleanup() {
    echo ""
    echo "Stopping ION container..."
    docker stop -t 2 ion-interop-test 2>/dev/null || true
    docker rm -f ion-interop-test 2>/dev/null || true
    echo "Cleanup complete"
    exit 0
}

trap cleanup INT TERM

# Build image if needed
if ! docker image inspect "$ION_IMAGE" &>/dev/null; then
    echo "Building ion-interop image (this may take a while)..."
    docker build -t "$ION_IMAGE" "$SCRIPT_DIR/docker"
fi

docker rm -f ion-interop-test 2>/dev/null || true

echo "Starting ION (ipn:$ION_NODE_NUM.0) with STCP on port $ION_STCP_PORT..."
docker run --rm \
    --name ion-interop-test \
    --network host \
    --ipc=host \
    -e ION_NODE_NUM="$ION_NODE_NUM" \
    -e STCP_PORT="$ION_STCP_PORT" \
    -e REMOTE_HOST="127.0.0.1" \
    -e REMOTE_PORT="$HARDY_STCP_PORT" \
    -e REMOTE_NODE="$HARDY_NODE_NUM" \
    "$ION_IMAGE" &

CONTAINER_PID=$!
echo "Waiting for ION to initialize..."
sleep 8

# Start echo service
echo "Starting bpecho on ipn:$ION_NODE_NUM.7..."
docker exec -d ion-interop-test bpecho "ipn:$ION_NODE_NUM.7"

sleep 3

echo ""
echo "============================================"
echo "ION ready for testing"
echo ""
echo "  Node:     ipn:$ION_NODE_NUM.0"
echo "  Echo:     ipn:$ION_NODE_NUM.7"
echo "  STCP:     127.0.0.1:$ION_STCP_PORT"
echo ""
echo "Test with:"
echo "  bp ping ipn:$ION_NODE_NUM.7 \\"
echo "    --cla /path/to/libhardy_mtcp_cla.so \\"
echo "    --cla-config '{\"framing\":\"stcp\",\"peer\":\"127.0.0.1:$ION_STCP_PORT\",\"peer-node\":\"ipn:$ION_NODE_NUM.0\",\"address\":\"0.0.0.0:$HARDY_STCP_PORT\"}' \\"
echo "    --source ipn:$HARDY_NODE_NUM.12345 --no-sign"
echo ""
echo "Press Ctrl+C to stop..."
echo "============================================"

docker wait ion-interop-test || true
cleanup
