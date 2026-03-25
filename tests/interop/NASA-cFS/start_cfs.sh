#!/bin/bash
# Interactive launcher for NASA cFS BPNode interop testing container.
#
# Builds the Docker image if needed (cloning from GitHub), starts the
# container with STCP CLA, and waits for Ctrl+C to stop.
#
# Usage:
#   ./tests/interop/NASA-cFS/start_cfs.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE_NAME="cfs-interop"
CONTAINER_NAME="cfs-interop-test"

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'
log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }

cleanup() {
    log_info "Stopping cFS container..."
    docker stop -t 2 "$CONTAINER_NAME" 2>/dev/null || true
    docker rm -f "$CONTAINER_NAME" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Build image if needed
if ! docker image inspect "$IMAGE_NAME" &>/dev/null; then
    log_info "Building $IMAGE_NAME Docker image (this may take a while)..."
    docker build -t "$IMAGE_NAME" "$SCRIPT_DIR/docker"
else
    log_info "Using existing $IMAGE_NAME image"
fi

# Clean up any existing container
docker rm -f "$CONTAINER_NAME" 2>/dev/null || true

log_info "Starting cFS BPNode (STCP echo mode)"
log_info "  STCP listen: 4501 (CLA In)"
log_info "  STCP connect: 127.0.0.1:4551 (CLA Out)"
log_info "  Echo: ipn:100.42 -> SB -> ipn:1.128"

docker run -d \
    --name "$CONTAINER_NAME" \
    --network host \
    --privileged \
    "$IMAGE_NAME"

log_info "Container started. Press Ctrl+C to stop."
log_info ""
log_info "To test manually (from hardy workspace):"
log_info "  bp ping ipn:10.7 --cla tests/interop/mtcp/target/release/mtcp-cla \\"
log_info "    --cla-args '--config stcp.toml' --grpc-listen '[::1]:50051' \\"
log_info "    --source ipn:1.12345 --count 5 --no-sign"

# Follow logs until interrupted
docker logs -f "$CONTAINER_NAME"
