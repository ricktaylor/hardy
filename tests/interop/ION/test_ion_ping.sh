#!/bin/bash
# Interoperability test: Hardy <-> ION ping/echo via STCP
#
# This script tests bidirectional ping/echo between Hardy and ION:
#   1. ION as server with bpecho, Hardy pings it via STCP
#   2. Hardy as server with echo service, ION bping pings it via STCP
#
# Prerequisites:
#   - Docker installed (for ION container)
#   - Hardy tools and bpa-server built
#   - MTCP/STCP CLA binary built (tests/interop/mtcp/)
#   - ION Docker image built (ion-interop)
#
# Usage:
#   ./tests/interop/ION/test_ion_ping.sh [--skip-build] [--no-docker]
#
# Options:
#   --skip-build   Skip building Hardy and CLA binaries
#   --no-docker    Use local ION binaries instead of Docker

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INTEROP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
MTCP_CLA_DIR="$INTEROP_DIR/mtcp"

# Configuration
HARDY_NODE_NUM=1
ION_NODE_NUM=2
ION_STCP_PORT=4557
HARDY_STCP_PORT=4558
HARDY_GRPC_PORT=50051
ION_IMAGE="ion-interop"
PING_COUNT=5

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
        --count|-c)
            PING_COUNT="$2"
            shift 2
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Cleanup function
ION_CONTAINER=""
HARDY_PID=""
CLA_PID=""
CLEANUP_IN_PROGRESS=""

# Helper to kill a process with SIGTERM, then SIGKILL if needed
kill_process() {
    local pid=$1
    local name=$2
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        log_info "Stopping $name (PID $pid)..."
        kill "$pid" 2>/dev/null || true
        local count=0
        while kill -0 "$pid" 2>/dev/null && [ $count -lt 50 ]; do
            sleep 0.1
            count=$((count + 1))
        done
        if kill -0 "$pid" 2>/dev/null; then
            log_warn "Force killing $name (PID $pid)..."
            kill -9 "$pid" 2>/dev/null || true
        fi
        wait "$pid" 2>/dev/null || true
    fi
}

cleanup() {
    if [ -n "$CLEANUP_IN_PROGRESS" ]; then
        return
    fi
    CLEANUP_IN_PROGRESS=1

    log_info "Cleaning up..."

    if [ -n "$ION_CONTAINER" ]; then
        docker stop -t 2 "$ION_CONTAINER" 2>/dev/null || true
        docker rm -f "$ION_CONTAINER" 2>/dev/null || true
    fi
    docker rm -f ion-interop-test 2>/dev/null || true

    kill_process "$CLA_PID" "mtcp-cla"
    kill_process "$HARDY_PID" "hardy-bpa-server"

    if [ -n "$TEST_DIR" ] && [ -d "$TEST_DIR" ]; then
        rm -rf "$TEST_DIR"
    fi

    log_info "Cleanup complete"
}
trap cleanup EXIT INT TERM

# Create temporary directory
TEST_DIR=$(mktemp -d)
log_info "Using test directory: $TEST_DIR"

# Build Hardy tools, server, and MTCP CLA binary if needed
if [ "$SKIP_BUILD" = false ]; then
    log_step "Building Hardy tools and bpa-server..."
    cd "$WORKSPACE_DIR"
    cargo build --release -p hardy-tools -p hardy-bpa-server

    log_step "Building MTCP/STCP CLA binary..."
    cd "$MTCP_CLA_DIR"
    cargo build --release
fi

BP_BIN="$WORKSPACE_DIR/target/release/bp"
BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"
CLA_BIN="$MTCP_CLA_DIR/target/release/mtcp-cla"

if [ ! -x "$BP_BIN" ]; then
    log_error "bp binary not found at $BP_BIN"
    exit 1
fi

if [ ! -x "$CLA_BIN" ]; then
    log_error "mtcp-cla binary not found at $CLA_BIN"
    log_error "Build it with: cd $MTCP_CLA_DIR && cargo build --release"
    exit 1
fi

# Build or check for ION
if [ "$USE_DOCKER" = true ]; then
    log_step "Checking for ion-interop Docker image..."
    if ! docker image inspect "$ION_IMAGE" &>/dev/null; then
        log_info "Building ion-interop Docker image (this may take a while)..."
        docker build -t "$ION_IMAGE" "$SCRIPT_DIR/docker"
    else
        log_info "Using existing ion-interop image"
    fi
else
    if ! command -v ionstart &> /dev/null; then
        log_error "ION not found in PATH"
        log_error "Install ION or use Docker mode"
        exit 1
    fi
    log_info "Found ION at: $(which ionstart)"
fi

# =============================================================================
# TEST 1: ION as server with bpecho, Hardy pings it via STCP
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 1: ION server with bpecho, Hardy pings via STCP"
echo "============================================================"

log_step "Starting ION daemon with STCP CL..."

if [ "$USE_DOCKER" = true ]; then
    docker rm -f ion-interop-test 2>/dev/null || true

    ION_CONTAINER=$(docker run -d \
        --name ion-interop-test \
        --network host \
        --ipc=host \
        -e ION_NODE_NUM="$ION_NODE_NUM" \
        -e STCP_PORT="$ION_STCP_PORT" \
        -e REMOTE_HOST="127.0.0.1" \
        -e REMOTE_PORT="$HARDY_STCP_PORT" \
        -e REMOTE_NODE="$HARDY_NODE_NUM" \
        "$ION_IMAGE")

    log_info "Started ION container: ${ION_CONTAINER:0:12}"

    log_info "Waiting for ION to initialize..."
    WAIT_TIMEOUT=30
    WAIT_COUNT=0
    while [ $WAIT_COUNT -lt $WAIT_TIMEOUT ]; do
        if ! docker ps -q -f "id=$ION_CONTAINER" | grep -q .; then
            log_error "ION container exited unexpectedly. Logs:"
            docker logs "$ION_CONTAINER" 2>&1 | tail -50
            docker rm "$ION_CONTAINER" 2>/dev/null || true
            exit 1
        fi

        if ss -tln 2>/dev/null | grep -q ":$ION_STCP_PORT "; then
            log_info "ION is listening on port $ION_STCP_PORT (took ${WAIT_COUNT}s)"
            break
        fi

        sleep 1
        WAIT_COUNT=$((WAIT_COUNT + 1))
    done

    # Give ION time to finish internal setup after port opens
    sleep 2

    if [ $WAIT_COUNT -ge $WAIT_TIMEOUT ]; then
        log_error "ION did not start listening on port $ION_STCP_PORT within ${WAIT_TIMEOUT}s"
        docker logs "$ION_CONTAINER" 2>&1 | tail -30
        exit 1
    fi

    # Start bpecho service
    log_step "Starting bpecho service on ipn:$ION_NODE_NUM.7..."
    docker exec -d "$ION_CONTAINER" bpecho "ipn:$ION_NODE_NUM.7"

    sleep 2
else
    log_error "Native ION mode not yet implemented - use Docker mode"
    exit 1
fi

# Create CLA config for bp ping (TEST 1)
cat > "$TEST_DIR/cla_ping.toml" << EOF
bpa-address = "http://[::1]:$HARDY_GRPC_PORT"
cla-name = "cl0"
framing = "stcp"
peer = "127.0.0.1:$ION_STCP_PORT"
peer-node = "ipn:$ION_NODE_NUM.0"
address = "0.0.0.0:$HARDY_STCP_PORT"
EOF

# Hardy pings ION echo service via STCP using the external CLA binary
log_step "Hardy pinging ION echo service at ipn:$ION_NODE_NUM.7 via STCP..."
echo ""

PING_OUTPUT=$(timeout 30s "$BP_BIN" ping "ipn:$ION_NODE_NUM.7" \
    --cla "$CLA_BIN" \
    --cla-args "--config $TEST_DIR/cla_ping.toml" \
    --grpc-listen "[::1]:$HARDY_GRPC_PORT" \
    --source "ipn:$HARDY_NODE_NUM.12345" \
    --count "$PING_COUNT" \
    --no-sign \
    2>&1) && EXIT_CODE=0 || EXIT_CODE=$?

echo "$PING_OUTPUT"
echo ""

STATS_LINE=$(echo "$PING_OUTPUT" | grep -E '[0-9]+ (bundles )?transmitted' | head -1)
TRANSMITTED=$(echo "$STATS_LINE" | sed -E 's/^([0-9]+).*/\1/')
RECEIVED=$(echo "$STATS_LINE" | sed -E 's/.*, ([0-9]+) received.*/\1/')

if [ $EXIT_CODE -eq 0 ]; then
    if [ "$RECEIVED" = "$TRANSMITTED" ] && [ -n "$RECEIVED" ]; then
        log_info "TEST 1 PASSED: Hardy successfully pinged ION ($RECEIVED/$TRANSMITTED)"
        TEST1_RESULT="PASS"
    else
        log_error "TEST 1 FAILED: Partial loss - only $RECEIVED/$TRANSMITTED responses received"
        TEST1_RESULT="FAIL"
    fi
elif [ $EXIT_CODE -eq 1 ]; then
    log_error "TEST 1 FAILED: No echo responses received (100% loss)"
    TEST1_RESULT="FAIL"
else
    log_error "TEST 1 FAILED: Error during ping (exit code $EXIT_CODE)"
    TEST1_RESULT="FAIL"
fi

# Stop ION for test 2
log_info "Stopping ION..."
if [ "$USE_DOCKER" = true ]; then
    docker stop "$ION_CONTAINER" 2>/dev/null || true
    docker rm -f "$ION_CONTAINER" 2>/dev/null || true
    ION_CONTAINER=""
    # Clean up stale ION shared memory from --ipc=host
    docker run --rm --ipc=host --entrypoint killm "$ION_IMAGE" 2>/dev/null || true
fi

sleep 1

# =============================================================================
# TEST 2: Hardy as server with echo, ION bping pings it via STCP
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 2: Hardy server with echo, ION pings via STCP"
echo "============================================================"

# Create Hardy bpa-server config
cat > "$TEST_DIR/hardy_config.toml" << EOF
log-level = "info"
status-reports = true
node-ids = "ipn:$HARDY_NODE_NUM.0"

[built-in-services]
echo = [7]

[storage.metadata]
type = "memory"

[storage.bundle]
type = "memory"

[rfc9171-validity]
primary-block-integrity = false

[grpc]
address = "[::1]:$HARDY_GRPC_PORT"
services = ["cla"]
EOF

# Create CLA config for the standalone STCP CLA process
cat > "$TEST_DIR/cla_server.toml" << EOF
bpa-address = "http://[::1]:$HARDY_GRPC_PORT"
cla-name = "cl0"
framing = "stcp"
address = "[::]:$HARDY_STCP_PORT"
peer = "127.0.0.1:$ION_STCP_PORT"
peer-node = "ipn:$ION_NODE_NUM.0"
EOF

log_step "Starting Hardy BPA server..."
"$BPA_BIN" -c "$TEST_DIR/hardy_config.toml" &
HARDY_PID=$!

sleep 2

if ! kill -0 "$HARDY_PID" 2>/dev/null; then
    log_error "Hardy BPA server failed to start"
    exit 1
fi
log_info "Hardy BPA server started with PID $HARDY_PID"

log_step "Starting MTCP/STCP CLA process..."
"$CLA_BIN" --config "$TEST_DIR/cla_server.toml" &
CLA_PID=$!

sleep 2

if ! kill -0 "$CLA_PID" 2>/dev/null; then
    log_error "MTCP/STCP CLA failed to start"
    exit 1
fi
log_info "MTCP/STCP CLA started with PID $CLA_PID"

# Start ION to ping Hardy
log_step "Starting ION to ping Hardy..."

if [ "$USE_DOCKER" = true ]; then
    docker rm -f ion-interop-test 2>/dev/null || true

    ION_CONTAINER=$(docker run -d \
        --name ion-interop-test \
        --network host \
        --ipc=host \
        -e ION_NODE_NUM="$ION_NODE_NUM" \
        -e STCP_PORT="$ION_STCP_PORT" \
        -e REMOTE_HOST="127.0.0.1" \
        -e REMOTE_PORT="$HARDY_STCP_PORT" \
        -e REMOTE_NODE="$HARDY_NODE_NUM" \
        "$ION_IMAGE")

    log_info "Started ION container: ${ION_CONTAINER:0:12}"

    log_info "Waiting for ION to initialize..."
    WAIT_TIMEOUT=30
    WAIT_COUNT=0
    while [ $WAIT_COUNT -lt $WAIT_TIMEOUT ]; do
        if ! docker ps -q -f "id=$ION_CONTAINER" | grep -q .; then
            break
        fi
        if ss -tln 2>/dev/null | grep -q ":$ION_STCP_PORT "; then
            log_info "ION is listening on port $ION_STCP_PORT (took ${WAIT_COUNT}s)"
            break
        fi
        sleep 1
        WAIT_COUNT=$((WAIT_COUNT + 1))
    done

    # Give ION time to finish internal setup after port opens
    sleep 2

    if ! docker ps -q -f "id=$ION_CONTAINER" | grep -q .; then
        log_error "ION container exited unexpectedly. Logs:"
        docker logs "$ION_CONTAINER" 2>&1 | tail -20
        docker rm "$ION_CONTAINER" 2>/dev/null || true
        TEST2_RESULT="FAIL"
    else
        # Run bping from ION container
        log_step "ION bping to Hardy echo service at ipn:$HARDY_NODE_NUM.7..."
        PING_TIMEOUT=$((PING_COUNT * 2 + 10))
        PING_OUTPUT=$(timeout "${PING_TIMEOUT}s" docker exec "$ION_CONTAINER" \
            bping -c "$PING_COUNT" -q 5 \
            "ipn:$ION_NODE_NUM.1" "ipn:$HARDY_NODE_NUM.7" \
            2>&1) || true

        echo "$PING_OUTPUT"
        echo ""

        # bping reports "N bundles transmitted, M bundles received, X% bundle loss"
        STATS_LINE=$(echo "$PING_OUTPUT" | grep "bundle loss" | head -1)
        RECEIVED=$(echo "$STATS_LINE" | sed -E 's/.*, ([0-9]+) bundles received.*/\1/')

        if [ -n "$RECEIVED" ] && [ "$RECEIVED" -ge 1 ] 2>/dev/null; then
            if echo "$STATS_LINE" | grep -q "0.00% bundle loss"; then
                log_info "TEST 2 PASSED: ION received $RECEIVED responses from Hardy"
                TEST2_RESULT="PASS"
            else
                log_error "TEST 2 FAILED: Partial loss ($STATS_LINE)"
                TEST2_RESULT="FAIL"
            fi
        else
            log_error "TEST 2 FAILED: No echo responses received"
            TEST2_RESULT="FAIL"
        fi
    fi
fi

# =============================================================================
# Summary
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST SUMMARY"
echo "============================================================"
echo ""
echo "  TEST 1 (Hardy pings ION via STCP): $TEST1_RESULT"
echo "  TEST 2 (ION pings Hardy via STCP): $TEST2_RESULT"
echo ""

if [ "$TEST1_RESULT" = "PASS" ] && [ "$TEST2_RESULT" = "PASS" ]; then
    log_info "All ION interoperability tests passed"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
