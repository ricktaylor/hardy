#!/bin/bash
# Start ud3tn in Docker for interactive testing via MTCP
#
# Usage:
#   ./tests/interop/ud3tn/start_ud3tn.sh
#
# Then in another terminal, create a CLA config file (e.g. /tmp/cla.toml):
#   bpa-address = "http://[::1]:50051"
#   cla-name = "cl0"
#   framing = "mtcp"
#   peer = "127.0.0.1:4557"
#   peer-node = "ipn:2.0"
#   address = "[::]:4558"
#
# Then run:
#   bp ping ipn:2.7 \
#       --cla /path/to/mtcp-cla \
#       --cla-args "--config /tmp/cla.toml" \
#       --grpc-listen "[::1]:50051" \
#       --source ipn:1.12345 --no-sign

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

UD3TN_IMAGE="ud3tn-interop"
UD3TN_NODE_NUM=2
UD3TN_MTCP_PORT=4557
UD3TN_AAP2_PORT=4243
HARDY_NODE_NUM=1
HARDY_MTCP_PORT=4558
HARDY_GRPC_PORT=50051

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

# Start echo agent (ud3tn doesn't ship one, so we create it inline).
# Uses two AAP2 connections (subscriber for recv, active for send)
# because ud3tn's subscriber mode is receive-only.
# Must send RESPONSE_STATUS_SUCCESS (1) after each received ADU.
echo "Starting echo agent on ipn:$UD3TN_NODE_NUM.7..."
docker exec -d ud3tn-interop-test \
    python3 -c "
from ud3tn_utils.aap2 import AAP2TCPClient, BundleADU
recv_client = AAP2TCPClient(('127.0.0.1', $UD3TN_AAP2_PORT))
recv_client.connect()
secret = recv_client.configure('7', subscribe=True)
send_client = AAP2TCPClient(('127.0.0.1', $UD3TN_AAP2_PORT))
send_client.connect()
send_client.configure('7', subscribe=False, secret=secret)
while True:
    msg = recv_client.receive_msg()
    t = msg.WhichOneof('msg')
    if t == 'keepalive':
        recv_client.send_response_status(2)
        continue
    if t != 'adu':
        continue
    adu, data = recv_client.receive_adu(msg.adu)
    recv_client.send_response_status(1)
    send_client.send_adu(BundleADU(dst_eid=adu.src_eid, payload_length=len(data)), data)
    send_client.receive_response()
" || echo "Warning: echo agent exited"

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
echo "Create a CLA config file (e.g. /tmp/cla.toml):"
echo "  bpa-address = \"http://[::1]:$HARDY_GRPC_PORT\""
echo "  cla-name = \"cl0\""
echo "  framing = \"mtcp\""
echo "  peer = \"127.0.0.1:$UD3TN_MTCP_PORT\""
echo "  peer-node = \"ipn:$UD3TN_NODE_NUM.0\""
echo "  address = \"[::]:$HARDY_MTCP_PORT\""
echo ""
echo "Then test with:"
echo "  bp ping ipn:$UD3TN_NODE_NUM.7 \\"
echo "    --cla /path/to/mtcp-cla \\"
echo "    --cla-args \"--config /tmp/cla.toml\" \\"
echo "    --grpc-listen \"[::1]:$HARDY_GRPC_PORT\" \\"
echo "    --source ipn:$HARDY_NODE_NUM.12345 --no-sign"
echo ""
echo "Press Ctrl+C to stop..."
echo "============================================"

docker wait ud3tn-interop-test || true
cleanup
