#!/bin/bash
# Start ud3tn in Docker for interactive testing via MTCP
#
# Usage:
#   ./tests/interop/ud3tn/start_ud3tn.sh
#
# Then in another terminal:
#   bp ping ipn:2.7 --cla /path/to/libhardy_mtcp_cla.so \
#       --cla-config '{"framing":"mtcp","peer":"127.0.0.1:4556","peer-node":"ipn:2.0","address":"[::]:4557"}' \
#       --source ipn:1.12345 --no-sign

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

UD3TN_IMAGE="ud3tn-interop"
UD3TN_NODE_NUM=2
UD3TN_MTCP_PORT=4556
UD3TN_AAP2_PORT=4243
HARDY_NODE_NUM=1
HARDY_MTCP_PORT=4557

cleanup() {
    echo ""
    echo "Stopping ud3tn container..."
    docker stop -t 2 ud3tn-interop-test 2>/dev/null || true
    docker rm -f ud3tn-interop-test 2>/dev/null || true
    echo "Cleanup complete"
    exit 0
}

trap cleanup INT TERM

# Build image if needed
if ! docker image inspect "$UD3TN_IMAGE" &>/dev/null; then
    echo "Building ud3tn-interop image..."
    docker build -t "$UD3TN_IMAGE" "$SCRIPT_DIR/docker"
fi

docker rm -f ud3tn-interop-test 2>/dev/null || true

echo "Starting ud3tn (ipn:$UD3TN_NODE_NUM.0) with MTCP on port $UD3TN_MTCP_PORT..."
docker run --rm \
    --name ud3tn-interop-test \
    --network host \
    "$UD3TN_IMAGE" \
    -e "ipn:$UD3TN_NODE_NUM.0" \
    -c "mtcp:0.0.0.0,$UD3TN_MTCP_PORT" \
    -b 7 \
    -A 0.0.0.0 -P "$UD3TN_AAP2_PORT" \
    -R &

CONTAINER_PID=$!
sleep 3

# Start echo agent
echo "Starting echo agent on ipn:$UD3TN_NODE_NUM.7..."
docker exec -d ud3tn-interop-test \
    python3 -m ud3tn_utils.aap.bin.aap_echo \
    --agentid 7 \
    --tcp 127.0.0.1 4242 \
    2>/dev/null || echo "Warning: could not start echo agent via AAP1"

# Configure contact to Hardy
echo "Configuring contact to Hardy (ipn:$HARDY_NODE_NUM.0)..."
docker exec ud3tn-interop-test \
    python3 -m ud3tn_utils.aap2.bin.aap2_configure_link \
    --tcp 127.0.0.1 "$UD3TN_AAP2_PORT" \
    "ipn:$HARDY_NODE_NUM.0" \
    "mtcp:127.0.0.1:$HARDY_MTCP_PORT" \
    2>/dev/null || echo "Warning: could not configure contact"

sleep 1

echo ""
echo "============================================"
echo "ud3tn ready for testing"
echo ""
echo "  Node:     ipn:$UD3TN_NODE_NUM.0"
echo "  Echo:     ipn:$UD3TN_NODE_NUM.7"
echo "  MTCP:     127.0.0.1:$UD3TN_MTCP_PORT"
echo "  AAP2:     127.0.0.1:$UD3TN_AAP2_PORT"
echo ""
echo "Test with:"
echo "  bp ping ipn:$UD3TN_NODE_NUM.7 \\"
echo "    --cla /path/to/libhardy_mtcp_cla.so \\"
echo "    --cla-config '{\"framing\":\"mtcp\",\"peer\":\"127.0.0.1:$UD3TN_MTCP_PORT\",\"peer-node\":\"ipn:$UD3TN_NODE_NUM.0\",\"address\":\"[::]:$HARDY_MTCP_PORT\"}' \\"
echo "    --source ipn:$HARDY_NODE_NUM.12345 --no-sign"
echo ""
echo "Press Ctrl+C to stop..."
echo "============================================"

docker wait ud3tn-interop-test || true
cleanup
