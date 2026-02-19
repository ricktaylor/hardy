#!/bin/bash
# Interoperability test: Hardy <-> dtn7-rs ping/echo
#
# This script tests bidirectional ping/echo between Hardy and dtn7-rs:
#   1. dtn7-rs as server with echo service, Hardy pings it
#   2. Hardy as server with echo service, dtn7-rs pings it
#
# Prerequisites:
#   - Docker installed (for dtn7-rs container)
#   - Hardy tools and bpa-server built
#
# Usage:
#   ./tests/interop/dtn7-rs/test_dtn7rs_ping.sh [--skip-build] [--no-docker]
#
# Options:
#   --skip-build   Skip building Hardy binaries
#   --no-docker    Use local dtnd/dtnecho2 instead of Docker

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Configuration
HARDY_NODE_NUM=1
DTN7_NODE_NUM=23
DTN7_PORT=4556
HARDY_PORT=4557
DTN7_WS_PORT=3000
DTN7_IMAGE="dtn7-interop"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }
log_step() { echo -e "${BLUE}[STEP]${NC} $*"; }

# Parse options
SKIP_BUILD=false
USE_DOCKER=true
while [[ $# -gt 0 ]]; do
    case $1 in
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        --no-docker)
            USE_DOCKER=false
            shift
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Cleanup function
DTN7_CONTAINER=""
DTND_PID=""
DTNECHO_PID=""
HARDY_PID=""
CLEANUP_IN_PROGRESS=""

# Helper to kill a process with SIGTERM, then SIGKILL if needed
kill_process() {
    local pid=$1
    local name=$2
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        log_info "Stopping $name (PID $pid)..."
        kill "$pid" 2>/dev/null || true
        # Wait up to 3 seconds for graceful shutdown
        local count=0
        while kill -0 "$pid" 2>/dev/null && [ $count -lt 30 ]; do
            sleep 0.1
            count=$((count + 1))
        done
        # Force kill if still running
        if kill -0 "$pid" 2>/dev/null; then
            log_warn "Force killing $name (PID $pid)..."
            kill -9 "$pid" 2>/dev/null || true
        fi
        wait "$pid" 2>/dev/null || true
    fi
}

cleanup() {
    # Prevent re-entrant cleanup
    if [ -n "$CLEANUP_IN_PROGRESS" ]; then
        return
    fi
    CLEANUP_IN_PROGRESS=1

    log_info "Cleaning up..."

    # Stop and remove Docker container
    if [ -n "$DTN7_CONTAINER" ]; then
        docker stop -t 2 "$DTN7_CONTAINER" 2>/dev/null || true
        docker rm -f "$DTN7_CONTAINER" 2>/dev/null || true
    fi
    # Also clean up by name in case container ID wasn't captured
    docker rm -f dtn7-interop-test 2>/dev/null || true

    # Stop native processes with graceful then forced kill
    kill_process "$DTNECHO_PID" "dtnecho2"
    kill_process "$DTND_PID" "dtnd"
    kill_process "$HARDY_PID" "hardy-bpa-server"

    if [ -n "$TEST_DIR" ] && [ -d "$TEST_DIR" ]; then
        rm -rf "$TEST_DIR"
    fi

    log_info "Cleanup complete"
}
# Trap EXIT, INT (Ctrl+C), and TERM signals for reliable cleanup
trap cleanup EXIT INT TERM

# Create temporary directory
TEST_DIR=$(mktemp -d)
log_info "Using test directory: $TEST_DIR"

# Build Hardy tools and server if needed
if [ "$SKIP_BUILD" = false ]; then
    log_step "Building Hardy tools and bpa-server..."
    cd "$WORKSPACE_DIR"
    cargo build --release -p hardy-tools -p hardy-bpa-server
fi

BP_BIN="$WORKSPACE_DIR/target/release/bp"
BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"

if [ ! -x "$BP_BIN" ]; then
    log_error "bp binary not found at $BP_BIN"
    exit 1
fi

# Build or check for dtn7-rs
if [ "$USE_DOCKER" = true ]; then
    log_step "Checking for dtn7-interop Docker image..."
    if ! docker image inspect "$DTN7_IMAGE" &>/dev/null; then
        log_info "Building dtn7-interop Docker image..."
        # Use docker directory as context - Dockerfile clones from GitHub, doesn't need workspace files
        docker build -f "$SCRIPT_DIR/docker/Dockerfile.dtn7-rs" -t "$DTN7_IMAGE" "$SCRIPT_DIR/docker"
    else
        log_info "Using existing dtn7-interop image"
    fi
else
    # Check for native dtn7-rs
    if ! command -v dtnd &> /dev/null; then
        log_error "dtnd (dtn7-rs) not found in PATH"
        log_error "Install with: cargo install dtn7"
        exit 1
    fi
    if ! command -v dtnecho2 &> /dev/null; then
        log_error "dtnecho2 not found in PATH"
        log_error "Build from dtn7-rs examples"
        exit 1
    fi
    log_info "Found dtnd at: $(which dtnd)"
fi

# =============================================================================
# TEST 1: dtn7-rs as server, Hardy pings it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 1: dtn7-rs server with echo, Hardy pings"
echo "============================================================"

log_step "Starting dtn7-rs daemon with TCPCLv4..."

if [ "$USE_DOCKER" = true ]; then
    # Clean up any existing container with the same name
    docker rm -f dtn7-interop-test 2>/dev/null || true

    # Run dtn7-rs in Docker
    # NODE_ID env var sets the node number for IPN naming (start_dtnd wrapper handles -n)
    # dtnd flags: -d (debug), -i0 (interval), -r epidemic (routing), -C tcp:port=PORT
    DTN7_CONTAINER=$(docker run -d \
        --name dtn7-interop-test \
        --network host \
        -e NODE_ID="$DTN7_NODE_NUM" \
        "$DTN7_IMAGE" \
        -d -i0 -r epidemic -C "tcp:port=$DTN7_PORT")

    log_info "Started dtn7-rs container: ${DTN7_CONTAINER:0:12}"

    # Wait for dtnd to start and be ready
    log_info "Waiting for dtnd to initialize..."
    sleep 3

    # Check if container is still running
    if ! docker ps -q -f "id=$DTN7_CONTAINER" | grep -q .; then
        log_error "dtn7-rs container exited unexpectedly. Logs:"
        docker logs "$DTN7_CONTAINER" 2>&1 | tail -50
        docker rm "$DTN7_CONTAINER" 2>/dev/null || true
        exit 1
    fi

    # Start dtnecho2 in the container
    log_step "Starting dtnecho2 service in container..."
    docker exec -d "$DTN7_CONTAINER" dtnecho2 -v

    # Give echo service time to connect
    sleep 2
else
    # Run dtn7-rs natively
    # dtnd: -d debug, -i0 interval, -r epidemic routing, -n node number, -C tcp convergence layer
    dtnd -d -i0 -r epidemic -n "$DTN7_NODE_NUM" -C "tcp:port=$DTN7_PORT" &
    DTND_PID=$!
    log_info "Started dtnd with PID $DTND_PID"

    sleep 3

    # Start echo service
    log_step "Starting dtnecho2 service..."
    dtnecho2 -v &
    DTNECHO_PID=$!
    log_info "Started dtnecho2 with PID $DTNECHO_PID"

    sleep 2
fi

# Verify dtn7-rs is running
if [ "$USE_DOCKER" = true ]; then
    if ! docker ps | grep -q dtn7-interop-test; then
        log_error "dtn7-rs container is not running"
        docker logs "$DTN7_CONTAINER" 2>&1 || true
        exit 1
    fi
fi

# Hardy pings dtn7-rs echo service (ipn:23.7)
log_step "Hardy pinging dtn7-rs echo service at ipn:$DTN7_NODE_NUM.7..."
echo ""

# Exit codes: 0=success (replies received), 1=no replies (100% loss), 2=error
# Use && true || true pattern to prevent set -e from exiting on non-zero
"$BP_BIN" ping "ipn:$DTN7_NODE_NUM.7" "127.0.0.1:$DTN7_PORT" \
    --count 5 \
    --no-sign \
    && EXIT_CODE=0 || EXIT_CODE=$?
if [ $EXIT_CODE -eq 0 ]; then
    log_info "TEST 1 PASSED: Hardy successfully pinged dtn7-rs"
    TEST1_RESULT="PASS"
elif [ $EXIT_CODE -eq 1 ]; then
    log_error "TEST 1 FAILED: No echo responses received (100% loss)"
    TEST1_RESULT="FAIL"
else
    log_error "TEST 1 FAILED: Error during ping (exit code $EXIT_CODE)"
    TEST1_RESULT="FAIL"
fi

# Stop dtn7-rs for test 2
log_info "Stopping dtn7-rs..."
if [ "$USE_DOCKER" = true ]; then
    docker stop "$DTN7_CONTAINER" 2>/dev/null || true
    docker rm -f "$DTN7_CONTAINER" 2>/dev/null || true
    DTN7_CONTAINER=""
else
    kill "$DTNECHO_PID" 2>/dev/null || true
    kill "$DTND_PID" 2>/dev/null || true
    wait "$DTNECHO_PID" 2>/dev/null || true
    wait "$DTND_PID" 2>/dev/null || true
    DTNECHO_PID=""
    DTND_PID=""
fi

sleep 1

# =============================================================================
# TEST 2: Hardy as server, dtn7-rs pings it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 2: Hardy server with echo, dtn7-rs pings"
echo "============================================================"

# Create Hardy config for server mode
cat > "$TEST_DIR/hardy_config.toml" << EOF
log_level = "info"
status_reports = true
node_ids = "ipn:$HARDY_NODE_NUM.0"

# Echo service on IPN service 7 only (no DTN since we only have IPN node ID)
echo = 7

[metadata_storage]
type = "memory"

[bundle_storage]
type = "memory"

[[clas]]
name = "cl0"
type = "tcpclv4"
address = "[::]:$HARDY_PORT"
# Disable TLS requirement for interop testing with dtn7-rs (plain TCP)
must_use_tls = false
EOF

log_step "Starting Hardy BPA server..."
"$BPA_BIN" -c "$TEST_DIR/hardy_config.toml" &
HARDY_PID=$!

sleep 3

if ! kill -0 "$HARDY_PID" 2>/dev/null; then
    log_error "Hardy BPA server failed to start"
    exit 1
fi
log_info "Hardy BPA server started with PID $HARDY_PID"

# Start dtn7-rs to ping Hardy
log_step "Starting dtn7-rs to ping Hardy..."

if [ "$USE_DOCKER" = true ]; then
    # Clean up any existing container with the same name
    docker rm -f dtn7-interop-test 2>/dev/null || true

    # Start dtn7-rs container
    DTN7_CONTAINER=$(docker run -d \
        --name dtn7-interop-test \
        --network host \
        -e NODE_ID="$DTN7_NODE_NUM" \
        "$DTN7_IMAGE" \
        -d -i0 -r epidemic -C "tcp:port=$DTN7_PORT")

    log_info "Started dtn7-rs container: ${DTN7_CONTAINER:0:12}"
    sleep 3

    # Check if container is still running
    if ! docker ps -q -f "id=$DTN7_CONTAINER" | grep -q .; then
        log_error "dtn7-rs container exited unexpectedly. Logs:"
        docker logs "$DTN7_CONTAINER" 2>&1 | tail -20
        docker rm "$DTN7_CONTAINER" 2>/dev/null || true
        TEST2_RESULT="FAIL"
    fi

    # Use dtnsend to send a bundle to Hardy's echo service
    log_step "dtn7-rs sending bundle to Hardy echo service at ipn:$HARDY_NODE_NUM.7..."

    # First, add Hardy as a peer via dtn7-rs REST API
    # Format: /peers/add?p=<PEER_URL>&p_t=<STATIC|DYNAMIC>
    # PEER_URL format for IPN: tcp://host:port/<node_number>
    log_info "Adding Hardy as peer at 127.0.0.1:$HARDY_PORT..."
    docker exec "$DTN7_CONTAINER" \
        curl -s "http://127.0.0.1:$DTN7_WS_PORT/peers/add?p=tcp://127.0.0.1:$HARDY_PORT/$HARDY_NODE_NUM&p_t=DYNAMIC" \
        2>&1 || log_warn "Could not add peer"

    sleep 1

    # Send bundle - payload via stdin, -r for receiver endpoint
    # Note: dtnsend -p is --port (websocket port), not payload. Payload comes from stdin.
    echo "ping from dtn7-rs" | docker exec -i "$DTN7_CONTAINER" \
        dtnsend -r "ipn:$HARDY_NODE_NUM.7" 2>&1 || true

    # Give time for round trip
    sleep 3

    # Check if Hardy received and echoed the bundle
    # This is harder to verify programmatically - check logs
    log_warn "TEST 2: Check Hardy logs for received bundle (manual verification needed)"
    TEST2_RESULT="MANUAL"
else
    # Native mode
    dtnd -d -i0 -r epidemic -n "$DTN7_NODE_NUM" -C "tcp:port=$DTN7_PORT" &
    DTND_PID=$!
    sleep 3

    # Add Hardy as peer via REST API
    # PEER_URL format for IPN: tcp://host:port/<node_number>
    log_info "Adding Hardy as peer at 127.0.0.1:$HARDY_PORT..."
    curl -s "http://127.0.0.1:$DTN7_WS_PORT/peers/add?p=tcp://127.0.0.1:$HARDY_PORT/$HARDY_NODE_NUM&p_t=DYNAMIC" \
        2>&1 || log_warn "Could not add peer"

    sleep 1

    log_step "dtn7-rs sending bundle to Hardy echo service..."
    # Payload via stdin, -r for receiver endpoint
    echo "ping from dtn7-rs" | dtnsend -r "ipn:$HARDY_NODE_NUM.7" 2>&1 || true

    sleep 3
    log_warn "TEST 2: Check Hardy logs for received bundle (manual verification needed)"
    TEST2_RESULT="MANUAL"
fi

# =============================================================================
# Summary
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST SUMMARY"
echo "============================================================"
echo ""
echo "  TEST 1 (Hardy pings dtn7-rs): $TEST1_RESULT"
echo "  TEST 2 (dtn7-rs pings Hardy): $TEST2_RESULT"
echo ""

if [ "$TEST1_RESULT" = "PASS" ]; then
    log_info "Interoperability test completed successfully"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
