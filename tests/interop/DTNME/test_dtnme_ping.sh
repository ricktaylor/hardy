#!/bin/bash
# Interoperability test: Hardy <-> DTNME ping/echo
#
# This script tests bidirectional ping/echo between Hardy and DTNME:
#   1. DTNME as server with echo service, Hardy pings it
#   2. Hardy as server with echo service, DTNME pings it
#
# Prerequisites:
#   - Docker installed (for DTNME container)
#   - Hardy tools and bpa-server built
#   - DTNME Docker image built (dtnme-interop)
#
# Usage:
#   ./tests/interop/DTNME/test_dtnme_ping.sh [--skip-build] [--no-docker]
#
# Options:
#   --skip-build   Skip building Hardy binaries
#   --no-docker    Use local DTNME binaries instead of Docker

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Configuration
HARDY_NODE_NUM=1
DTNME_NODE_NUM=2
DTNME_PORT=4556
HARDY_PORT=4557
DTNME_IMAGE="dtnme-interop"
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
DTNME_CONTAINER=""
DTNME_PID=""
DTNME_ECHO_PID=""
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
    if [ -n "$DTNME_CONTAINER" ]; then
        docker stop -t 2 "$DTNME_CONTAINER" 2>/dev/null || true
        docker rm -f "$DTNME_CONTAINER" 2>/dev/null || true
    fi
    # Also clean up by name in case container ID wasn't captured
    docker rm -f dtnme-interop-test 2>/dev/null || true

    # Stop native processes with graceful then forced kill
    kill_process "$DTNME_ECHO_PID" "dtnme-echo"
    kill_process "$DTNME_PID" "dtnme"
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

# Build or check for DTNME
if [ "$USE_DOCKER" = true ]; then
    log_step "Checking for dtnme-interop Docker image..."
    if ! docker image inspect "$DTNME_IMAGE" &>/dev/null; then
        log_info "Building dtnme-interop Docker image (this may take a while)..."
        docker build -t "$DTNME_IMAGE" "$SCRIPT_DIR/docker"
    else
        log_info "Using existing dtnme-interop image"
    fi
else
    # Check for native DTNME
    if ! command -v dtnme &> /dev/null; then
        log_error "dtnme not found in PATH"
        log_error "Install DTNME or use Docker mode"
        exit 1
    fi
    log_info "Found dtnme at: $(which dtnme)"
fi

# =============================================================================
# TEST 1: DTNME as server, Hardy pings it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 1: DTNME server with echo, Hardy pings"
echo "============================================================"

log_step "Starting DTNME daemon with TCP CL and static routing..."

if [ "$USE_DOCKER" = true ]; then
    # Clean up any existing container with the same name
    docker rm -f dtnme-interop-test 2>/dev/null || true

    # Run DTNME in Docker with flood routing enabled (default)
    # REMOTE_HOST must be set so DTNME creates a link back to Hardy
    # Even though Hardy initiates the connection, DTNME needs an outbound
    # link definition to route response bundles back
    DTNME_CONTAINER=$(docker run -d \
        --name dtnme-interop-test \
        --network host \
        -e NODE_ID="$DTNME_NODE_NUM" \
        -e TCPCL_PORT="$DTNME_PORT" \
        -e REMOTE_HOST="127.0.0.1" \
        -e REMOTE_PORT="$HARDY_PORT" \
        -e REMOTE_NODE="$HARDY_NODE_NUM" \
        "$DTNME_IMAGE")

    log_info "Started DTNME container: ${DTNME_CONTAINER:0:12}"

    # Wait for DTNME to start and be ready
    log_info "Waiting for DTNME to initialize..."
    sleep 5

    # Check if container is still running
    if ! docker ps -q -f "id=$DTNME_CONTAINER" | grep -q .; then
        log_error "DTNME container exited unexpectedly. Logs:"
        docker logs "$DTNME_CONTAINER" 2>&1 | tail -50
        docker rm "$DTNME_CONTAINER" 2>/dev/null || true
        exit 1
    fi

    # Start echo service in the container
    log_step "Starting echo_me service in container..."
    docker exec -d "$DTNME_CONTAINER" /dtn/bin/echo_me -B 5010 -s "ipn:$DTNME_NODE_NUM.7"

    # Give echo service time to start
    sleep 2
else
    log_error "Native DTNME mode not yet implemented - use Docker mode"
    exit 1
fi

# Verify DTNME is running
if [ "$USE_DOCKER" = true ]; then
    if ! docker ps | grep -q dtnme-interop-test; then
        log_error "DTNME container is not running"
        docker logs "$DTNME_CONTAINER" 2>&1 || true
        exit 1
    fi
fi

# Hardy pings DTNME echo service (ipn:2.7)
log_step "Hardy pinging DTNME echo service at ipn:$DTNME_NODE_NUM.7..."
echo ""

# Exit codes: 0=success (replies received), 1=no replies (100% loss), 2=error
# Use --source to specify an EID that matches DTNME's route (ipn:$HARDY_NODE_NUM.*)
# Note: --no-sign disables BIB signing (DTNME echo doesn't sign responses)
# Note: --no-payload-crc is needed because DTNME has a bug where it doesn't validate
#       payload block CRC but rejects bundles when CRC validation fails.
# Capture output to check actual received count
PING_OUTPUT=$("$BP_BIN" ping "ipn:$DTNME_NODE_NUM.7" "127.0.0.1:$DTNME_PORT" \
    --source "ipn:$HARDY_NODE_NUM.12345" \
    --count "$PING_COUNT" \
    --no-sign \
    --no-payload-crc \
    2>&1) && EXIT_CODE=0 || EXIT_CODE=$?

echo "$PING_OUTPUT"
echo ""

# Extract received count from "N bundles transmitted, M received" line
STATS_LINE=$(echo "$PING_OUTPUT" | grep -E '[0-9]+ (bundles )?transmitted' | head -1)
TRANSMITTED=$(echo "$STATS_LINE" | sed -E 's/^([0-9]+).*/\1/')
RECEIVED=$(echo "$STATS_LINE" | sed -E 's/.*,\s*([0-9]+)\s+received.*/\1/')

if [ $EXIT_CODE -eq 0 ]; then
    if [ "$RECEIVED" = "$TRANSMITTED" ] && [ -n "$RECEIVED" ]; then
        log_info "TEST 1 PASSED: Hardy successfully pinged DTNME ($RECEIVED/$TRANSMITTED)"
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

# Stop DTNME for test 2
log_info "Stopping DTNME..."
if [ "$USE_DOCKER" = true ]; then
    docker stop "$DTNME_CONTAINER" 2>/dev/null || true
    docker rm -f "$DTNME_CONTAINER" 2>/dev/null || true
    DTNME_CONTAINER=""
fi

sleep 1

# =============================================================================
# TEST 2: Hardy as server, DTNME pings it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 2: Hardy server with echo, DTNME pings"
echo "============================================================"

# Create Hardy config for server mode
cat > "$TEST_DIR/hardy_config.toml" << EOF
log_level = "info"
status_reports = true
node_ids = "ipn:$HARDY_NODE_NUM.0"

# Echo service on IPN service 7
echo = 7

[metadata_storage]
type = "memory"

[bundle_storage]
type = "memory"

[rfc9171-validity]
primary-block-integrity = false

[[clas]]
name = "cl0"
type = "tcpclv4"
address = "[::]:$HARDY_PORT"
# Disable TLS requirement for interop testing with DTNME (plain TCP)
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

# Start DTNME to ping Hardy
log_step "Starting DTNME to ping Hardy..."

if [ "$USE_DOCKER" = true ]; then
    # Clean up any existing container with the same name
    docker rm -f dtnme-interop-test 2>/dev/null || true

    # Start DTNME container with link to Hardy
    DTNME_CONTAINER=$(docker run -d \
        --name dtnme-interop-test \
        --network host \
        -e NODE_ID="$DTNME_NODE_NUM" \
        -e TCPCL_PORT="$DTNME_PORT" \
        -e REMOTE_HOST="127.0.0.1" \
        -e REMOTE_PORT="$HARDY_PORT" \
        -e REMOTE_NODE="$HARDY_NODE_NUM" \
        "$DTNME_IMAGE")

    log_info "Started DTNME container: ${DTNME_CONTAINER:0:12}"
    sleep 5

    # Check if container is still running
    if ! docker ps -q -f "id=$DTNME_CONTAINER" | grep -q .; then
        log_error "DTNME container exited unexpectedly. Logs:"
        docker logs "$DTNME_CONTAINER" 2>&1 | tail -20
        docker rm "$DTNME_CONTAINER" 2>/dev/null || true
        TEST2_RESULT="FAIL"
    else
        # Establish DTNME link to Hardy before pinging
        log_info "Establishing DTNME -> Hardy TCP connection..."
        docker exec "$DTNME_CONTAINER" /dtn/bin/send_me -s "ipn:$DTNME_NODE_NUM.1" -d "127.0.0.1:$HARDY_PORT" -p "link warmup" 2>/dev/null || true

        # Wait for connection to stabilize (prevents losing first pings)
        log_info "Waiting for connection to stabilize..."
        sleep 3

        # Run ping from DTNME container
        log_step "DTNME ping_me to Hardy echo service at ipn:$HARDY_NODE_NUM.7..."
        # -e 2: 2 second expiration (loopback is fast, connection already established)
        # -c N: send N pings (configurable via --count)
        # timeout 20s: safety net in case ping_me hangs
        PING_OUTPUT=$(timeout 20s docker exec "$DTNME_CONTAINER" /dtn/bin/ping_me \
            -B 5010 \
            -s "ipn:$DTNME_NODE_NUM.1" \
            -e 2 \
            -c "$PING_COUNT" \
            "ipn:$HARDY_NODE_NUM.7" \
            2>&1) || true

        echo "$PING_OUTPUT"
        echo ""

        # Count responses - look for "time=" which indicates a successful reply
        RESPONSE_COUNT=$(echo "$PING_OUTPUT" | grep -c "time=" || echo "0")

        if [ "$RESPONSE_COUNT" = "$PING_COUNT" ]; then
            log_info "TEST 2 PASSED: DTNME received $RESPONSE_COUNT/$PING_COUNT responses from Hardy"
            TEST2_RESULT="PASS"
        elif [ "$RESPONSE_COUNT" -ge 1 ]; then
            log_error "TEST 2 FAILED: Partial loss - only $RESPONSE_COUNT/$PING_COUNT responses received"
            TEST2_RESULT="FAIL"
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
echo "  TEST 1 (Hardy pings DTNME): $TEST1_RESULT"
echo "  TEST 2 (DTNME pings Hardy): $TEST2_RESULT"
echo ""

if [ "$TEST1_RESULT" = "PASS" ] && [ "$TEST2_RESULT" = "PASS" ]; then
    log_info "All interoperability tests passed"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
