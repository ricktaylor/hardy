#!/bin/bash
# Interactive launcher for ESA-BP interop testing container.
#
# Builds the Docker image if needed, starts the container with STCP CLE,
# and waits for Ctrl+C to stop.
#
# Usage:
#   ./tests/interop/ESA-BP/start_esa_bp.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ESA_BP_SRC="$(cd "${ESA_BP_SRC:-$SCRIPT_DIR/../../../../esa-bp}" 2>/dev/null && pwd || echo "${ESA_BP_SRC:-$SCRIPT_DIR/../../../../esa-bp}")"
BASE_IMAGE="esa-bp"
IMAGE_NAME="esa-bp-interop"
CONTAINER_NAME="esa-bp-interop-test"

# Configuration
NODE_ID="${NODE_ID:-10}"
STCP_LISTEN_PORT="${STCP_LISTEN_PORT:-4558}"
STCP_DEST_IP="${STCP_DEST_IP:-127.0.0.1}"
STCP_DEST_PORT="${STCP_DEST_PORT:-4557}"
REMOTE_NODE_ID="${REMOTE_NODE_ID:-1}"

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'
log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }

cleanup() {
    log_info "Stopping ESA-BP container..."
    docker stop -t 2 "$CONTAINER_NAME" 2>/dev/null || true
    docker rm -f "$CONTAINER_NAME" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# Build images if needed
if ! docker image inspect "$IMAGE_NAME" &>/dev/null; then
    if [ ! -d "$ESA_BP_SRC/src" ]; then
        log_warn "ESA-BP source not found at $ESA_BP_SRC"
        log_warn "Set ESA_BP_SRC to the ESA-BP source directory"
        exit 1
    fi

    # Build base ESA-BP image using their native Dockerfile
    if ! docker image inspect "$BASE_IMAGE" &>/dev/null; then
        log_info "Building base ESA-BP image (this may take a while)..."
        # Fix trailing slash on COPY destination (their Dockerfile bug)
        sed 's|COPY --from=builder /src/\*/target/\*distribution.zip /opt/esa-bp$|COPY --from=builder /src/*/target/*distribution.zip /opt/esa-bp/|' \
            "$ESA_BP_SRC/docker/Dockerfile" | \
            docker build -t "$BASE_IMAGE" -f - "$ESA_BP_SRC"
    fi

    # Layer our STCP CLE on top
    log_info "Building $IMAGE_NAME interop image..."
    docker build -t "$IMAGE_NAME" \
        --build-arg "BASE_IMAGE=$BASE_IMAGE" \
        -f "$SCRIPT_DIR/docker/Dockerfile" \
        "$SCRIPT_DIR"
else
    log_info "Using existing $IMAGE_NAME image"
fi

# Clean up any existing container
docker rm -f "$CONTAINER_NAME" 2>/dev/null || true

log_info "Starting ESA-BP node ipn:${NODE_ID}.0"
log_info "  STCP listen: ${STCP_LISTEN_PORT}"
log_info "  STCP dest:   ${STCP_DEST_IP}:${STCP_DEST_PORT}"
log_info "  Route:       ipn:${REMOTE_NODE_ID}.0 -> STCP"

docker run -d \
    --name "$CONTAINER_NAME" \
    --network host \
    -e NODE_ID="$NODE_ID" \
    -e STCP_LISTEN_PORT="$STCP_LISTEN_PORT" \
    -e STCP_DEST_IP="$STCP_DEST_IP" \
    -e STCP_DEST_PORT="$STCP_DEST_PORT" \
    -e REMOTE_NODE_ID="$REMOTE_NODE_ID" \
    "$IMAGE_NAME"

log_info "Container started. Press Ctrl+C to stop."
log_info ""
log_info "To test manually:"
log_info "  bp ping ipn:${NODE_ID}.7 127.0.0.1:${STCP_LISTEN_PORT} --source ipn:1.1 --count 5"

# Follow logs until interrupted
docker logs -f "$CONTAINER_NAME"
