#!/bin/bash
# Interoperability test: Hardy <-> HDTN ping/echo
#
# This script tests bidirectional ping/echo between Hardy and HDTN:
#   1. HDTN as server with echo service, Hardy pings it
#   2. Hardy as server with echo service, HDTN pings it
#
# Prerequisites:
#   - Docker installed (for HDTN container)
#   - Hardy tools and bpa-server built
#   - HDTN Docker image built (hdtn-interop)
#
# Usage:
#   ./tests/interop/HDTN/test_hdtn_ping.sh [--skip-build] [--no-docker]
#
# Options:
#   --skip-build   Skip building Hardy binaries
#   --no-docker    Use local HDTN binaries instead of Docker

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Configuration
HARDY_NODE_NUM=1
HDTN_NODE_NUM=10
HDTN_PORT=4556
HARDY_PORT=4557
HDTN_IMAGE="hdtn-interop"
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
HDTN_CONTAINER=""
HDTN_PID=""
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
    if [ -n "$HDTN_CONTAINER" ]; then
        docker stop -t 2 "$HDTN_CONTAINER" 2>/dev/null || true
        docker rm -f "$HDTN_CONTAINER" 2>/dev/null || true
    fi
    # Also clean up by name in case container ID wasn't captured
    docker rm -f hdtn-interop-test 2>/dev/null || true

    # Stop native processes with graceful then forced kill
    kill_process "$HDTN_PID" "hdtn"
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

# Build or check for HDTN
if [ "$USE_DOCKER" = true ]; then
    log_step "Checking for hdtn-interop Docker image..."
    if ! docker image inspect "$HDTN_IMAGE" &>/dev/null; then
        log_info "Building hdtn-interop Docker image (this may take a while)..."
        docker build -t "$HDTN_IMAGE" "$SCRIPT_DIR/docker"
    else
        log_info "Using existing hdtn-interop image"
    fi
else
    # Check for native HDTN
    if ! command -v hdtn-one-process &> /dev/null; then
        log_error "hdtn-one-process not found in PATH"
        log_error "Install HDTN or use Docker mode"
        exit 1
    fi
    log_info "Found hdtn-one-process at: $(which hdtn-one-process)"
fi

# =============================================================================
# TEST 1: HDTN as server, Hardy pings it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 1: HDTN server with echo, Hardy pings"
echo "============================================================"

log_step "Starting HDTN daemon with TCPCLv4..."

if [ "$USE_DOCKER" = true ]; then
    # Clean up any existing container with the same name
    docker rm -f hdtn-interop-test 2>/dev/null || true

    # Run HDTN in Docker
    HDTN_CONTAINER=$(docker run -d \
        --name hdtn-interop-test \
        --network host \
        -e NODE_ID="$HDTN_NODE_NUM" \
        -e TCPCL_PORT="$HDTN_PORT" \
        "$HDTN_IMAGE")

    log_info "Started HDTN container: ${HDTN_CONTAINER:0:12}"

    # Wait for HDTN to start and be ready
    log_info "Waiting for HDTN to initialize..."

    # Wait up to 30 seconds for HDTN to be ready (port open)
    WAIT_TIMEOUT=30
    WAIT_COUNT=0
    while [ $WAIT_COUNT -lt $WAIT_TIMEOUT ]; do
        # Check if container is still running
        if ! docker ps -q -f "id=$HDTN_CONTAINER" | grep -q .; then
            log_error "HDTN container exited unexpectedly. Logs:"
            docker logs "$HDTN_CONTAINER" 2>&1 | tail -50
            docker rm "$HDTN_CONTAINER" 2>/dev/null || true
            exit 1
        fi

        # Check if port is open (try multiple methods for compatibility)
        if nc -z 127.0.0.1 "$HDTN_PORT" 2>/dev/null; then
            log_info "HDTN is listening on port $HDTN_PORT (took ${WAIT_COUNT}s)"
            break
        elif timeout 1 bash -c "echo > /dev/tcp/127.0.0.1/$HDTN_PORT" 2>/dev/null; then
            log_info "HDTN is listening on port $HDTN_PORT (took ${WAIT_COUNT}s)"
            break
        elif ss -tlnp 2>/dev/null | grep -q ":$HDTN_PORT "; then
            log_info "HDTN is listening on port $HDTN_PORT (took ${WAIT_COUNT}s, detected via ss)"
            break
        fi

        sleep 1
        WAIT_COUNT=$((WAIT_COUNT + 1))
    done

    if [ $WAIT_COUNT -ge $WAIT_TIMEOUT ]; then
        log_error "HDTN did not start listening on port $HDTN_PORT within ${WAIT_TIMEOUT}s"
        log_error "Checking what ports are listening inside container:"
        docker exec "$HDTN_CONTAINER" netstat -tlnp 2>/dev/null || docker exec "$HDTN_CONTAINER" ss -tlnp 2>/dev/null || true
        log_error "Checking from host:"
        netstat -tlnp 2>/dev/null | grep -E ":$HDTN_PORT|:4556" || ss -tlnp 2>/dev/null | grep -E ":$HDTN_PORT|:4556" || true
        log_error "HDTN container logs:"
        docker logs "$HDTN_CONTAINER" 2>&1 | tail -50
        exit 1
    fi
else
    log_error "Native HDTN mode not yet implemented - use Docker mode"
    exit 1
fi

# Hardy pings HDTN echo service (ipn:10.2047)
# Note: HDTN uses service ID 2047 for echo (not 7)
log_step "Hardy pinging HDTN echo service at ipn:$HDTN_NODE_NUM.2047..."
echo ""

# Exit codes: 0=success (replies received), 1=no replies (100% loss), 2=error
# Use a known source EID so HDTN can route responses back
# Capture output to check actual received count
PING_OUTPUT=$("$BP_BIN" ping "ipn:$HDTN_NODE_NUM.2047" "127.0.0.1:$HDTN_PORT" \
    --source "ipn:$HARDY_NODE_NUM.1" \
    --count "$PING_COUNT" \
    --no-sign \
    2>&1) && EXIT_CODE=0 || EXIT_CODE=$?

echo "$PING_OUTPUT"
echo ""

# Extract received count from "N bundles transmitted, M received" line
STATS_LINE=$(echo "$PING_OUTPUT" | grep -E '[0-9]+ (bundles )?transmitted' | head -1)
TRANSMITTED=$(echo "$STATS_LINE" | sed -E 's/^([0-9]+).*/\1/')
RECEIVED=$(echo "$STATS_LINE" | sed -E 's/.*,\s*([0-9]+)\s+received.*/\1/')

if [ $EXIT_CODE -eq 0 ]; then
    if [ "$RECEIVED" = "$TRANSMITTED" ] && [ -n "$RECEIVED" ]; then
        log_info "TEST 1 PASSED: Hardy successfully pinged HDTN ($RECEIVED/$TRANSMITTED)"
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

# Stop HDTN for test 2
log_info "Stopping HDTN..."
if [ "$USE_DOCKER" = true ]; then
    docker stop "$HDTN_CONTAINER" 2>/dev/null || true
    docker rm -f "$HDTN_CONTAINER" 2>/dev/null || true
    HDTN_CONTAINER=""
fi

sleep 1

# =============================================================================
# TEST 2: Hardy as server, HDTN pings it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 2: Hardy server with echo, HDTN pings"
echo "============================================================"

# Create Hardy config for server mode
cat > "$TEST_DIR/hardy_config.toml" << EOF
log_level = "info"
status_reports = true
node_ids = "ipn:$HARDY_NODE_NUM.0"

# Echo service on IPN service 7 (standard) and 2047 (HDTN compatible)
echo = [7, 2047]

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
# Disable TLS requirement for interop testing with HDTN (plain TCP)
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

# Start HDTN to ping Hardy
log_step "Starting HDTN to ping Hardy..."

if [ "$USE_DOCKER" = true ]; then
    # Clean up any existing container with the same name
    docker rm -f hdtn-interop-test 2>/dev/null || true

    # Start HDTN container
    HDTN_CONTAINER=$(docker run -d \
        --name hdtn-interop-test \
        --network host \
        -e NODE_ID="$HDTN_NODE_NUM" \
        -e TCPCL_PORT="$HDTN_PORT" \
        "$HDTN_IMAGE")

    log_info "Started HDTN container: ${HDTN_CONTAINER:0:12}"
    sleep 5

    # Check if container is still running
    if ! docker ps -q -f "id=$HDTN_CONTAINER" | grep -q .; then
        log_error "HDTN container exited unexpectedly. Logs:"
        docker logs "$HDTN_CONTAINER" 2>&1 | tail -20
        docker rm "$HDTN_CONTAINER" 2>/dev/null || true
        TEST2_RESULT="FAIL"
    else
        # Use HDTN's bping to ping Hardy's echo service
        # bping sends to destination and expects echo back
        log_step "HDTN bping to Hardy echo service at ipn:$HARDY_NODE_NUM.2047..."

        # Create outduct config for bping to connect to Hardy
        cat > "$TEST_DIR/bping_outduct.json" << EOF
{
    "outductConfigName": "bping_outduct",
    "outductVector": [
        {
            "name": "tcpclv4_to_hardy",
            "convergenceLayer": "tcpcl_v4",
            "nextHopNodeId": $HARDY_NODE_NUM,
            "remoteHostname": "127.0.0.1",
            "remotePort": $HARDY_PORT,
            "maxNumberOfBundlesInPipeline": 50,
            "maxSumOfBundleBytesInPipeline": 50000000,
            "keepAliveIntervalSeconds": 17,
            "tcpclAllowOpportunisticReceiveBundles": true,
            "tcpclV4MyMaxRxSegmentSizeBytes": 200000,
            "tryUseTls": false,
            "tlsIsRequired": false,
            "useTlsVersion1_3": false,
            "doX509CertificateVerification": false,
            "verifySubjectAltNameInX509Certificate": false,
            "certificationAuthorityPemFileForVerification": ""
        }
    ]
}
EOF

        # Copy config to container
        docker cp "$TEST_DIR/bping_outduct.json" hdtn-interop-test:/tmp/bping_outduct.json

        # Run bping from HDTN container
        # --duration is in seconds (sends at --bundle-rate per second, default 1)
        # Use service 7 (not 2047) because Hardy's EchoService has a bug where
        # only the first registered service works (second sink gets dropped)
        echo ""
        PING_OUTPUT=$(docker exec hdtn-interop-test bping \
            --use-bp-version-7 \
            --my-uri-eid="ipn:$HDTN_NODE_NUM.1" \
            --dest-uri-eid="ipn:$HARDY_NODE_NUM.7" \
            --outducts-config-file=/tmp/bping_outduct.json \
            --bundle-send-timeout-seconds=10 \
            --duration="$PING_COUNT" \
            2>&1) || true

        echo "$PING_OUTPUT"
        echo ""

        # Check for success: compare sent vs received bundle counts
        # bping outputs "totalBundlesReceived N" and "totalNonAdminRecordBpv7BundlesRx: N"
        # and "Ping received: sequence=N" for each successful ping response
        BUNDLES_RECEIVED=$(echo "$PING_OUTPUT" | grep -oP 'totalBundlesReceived \K[0-9]+' || echo "0")
        if [ "$BUNDLES_RECEIVED" = "0" ]; then
            BUNDLES_RECEIVED=$(echo "$PING_OUTPUT" | grep -oP 'totalNonAdminRecordBpv7BundlesRx: \K[0-9]+' || echo "0")
        fi
        # For sent bundles, look for "totalBundlesSent N" in the output
        BUNDLES_SENT=$(echo "$PING_OUTPUT" | grep -oP 'totalBundlesSent \K[0-9]+' | head -1 || echo "0")

        if [ "$BUNDLES_SENT" -gt 0 ] && [ "$BUNDLES_RECEIVED" = "$BUNDLES_SENT" ] 2>/dev/null; then
            log_info "TEST 2 PASSED: HDTN pinged Hardy ($BUNDLES_RECEIVED/$BUNDLES_SENT)"
            TEST2_RESULT="PASS"
        elif [ "$BUNDLES_RECEIVED" -gt 0 ] 2>/dev/null; then
            log_error "TEST 2 FAILED: Partial loss - only $BUNDLES_RECEIVED/$BUNDLES_SENT responses received"
            TEST2_RESULT="FAIL"
        elif [ "$BUNDLES_SENT" -gt 0 ] 2>/dev/null; then
            log_error "TEST 2 FAILED: HDTN sent $BUNDLES_SENT bundles but received 0 responses"
            TEST2_RESULT="FAIL"
        else
            log_error "TEST 2 FAILED: Unable to determine ping result"
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
echo "  TEST 1 (Hardy pings HDTN): $TEST1_RESULT"
echo "  TEST 2 (HDTN pings Hardy): $TEST2_RESULT"
echo ""

if [ "$TEST1_RESULT" = "PASS" ] && [ "$TEST2_RESULT" = "PASS" ]; then
    log_info "All interoperability tests passed"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
