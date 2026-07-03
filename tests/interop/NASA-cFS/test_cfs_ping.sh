#!/bin/bash
# Interoperability test: Hardy <-> NASA cFS BPNode ping/echo
#
# This script tests bidirectional bundle exchange between Hardy and
# NASA's cFS BPNode implementation:
#   TEST 1: cFS as server with SB echo, Hardy pings it (via STCP CLA)
#   TEST 2: cFS as client, ping_app originates and counts Hardy's reflections
#
# The Hardy side uses the mtcp-cla binary in STCP mode as an external
# CLA for bp ping (--cla flag).
#
# Prerequisites:
#   - Docker installed (for cFS container)
#   - Hardy tools, bpa-server, and mtcp-cla built
#
# Usage:
#   ./tests/interop/NASA-cFS/test_cfs_ping.sh [--skip-build]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# shellcheck source=../lib/wait.sh
source "$SCRIPT_DIR/../lib/wait.sh"

# Configuration
HARDY_NODE_NUM=1
HARDY_SERVICE_NUM=128
CFS_NODE_NUM=100
CFS_STCP_PORT=4501
HARDY_STCP_PORT=4551
HARDY_GRPC_PORT=50051
CFS_IMAGE="cfs-interop"
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
REFRESH=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        --refresh)
            REFRESH=true
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
CFS_CONTAINER=""
HARDY_PID=""
MTCP_PID=""
CLEANUP_IN_PROGRESS=""

kill_process() {
    local pid=$1
    local name=$2
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        log_info "Stopping $name (PID $pid)..."
        kill "$pid" 2>/dev/null || true
        local count=0
        while kill -0 "$pid" 2>/dev/null && [ $count -lt 30 ]; do
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

    # Stop Docker container
    if [ -n "$CFS_CONTAINER" ]; then
        docker stop -t 2 "$CFS_CONTAINER" 2>/dev/null || true
        docker rm -f "$CFS_CONTAINER" 2>/dev/null || true
    fi
    docker rm -f cfs-interop-test 2>/dev/null || true

    kill_process "$MTCP_PID" "mtcp-cla"
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

# Build Hardy tools if needed
if [ "$SKIP_BUILD" = false ]; then
    log_step "Building Hardy tools and bpa-server..."
    cd "$WORKSPACE_DIR"
    cargo build --release -p hardy-tools -p hardy-bpa-server
    # mtcp-cla is excluded from the workspace — build it separately
    log_step "Building mtcp-cla..."
    cd "$WORKSPACE_DIR/tests/interop/mtcp"
    cargo build --release
fi

BP_BIN="$WORKSPACE_DIR/target/release/bp"
BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"
MTCP_CLA_BIN="$WORKSPACE_DIR/tests/interop/mtcp/target/release/mtcp-cla"

for bin in "$BP_BIN" "$BPA_BIN" "$MTCP_CLA_BIN"; do
    if [ ! -x "$bin" ]; then
        log_error "Binary not found: $bin"
        exit 1
    fi
done

# Build or check for cFS Docker image
log_step "Checking for $CFS_IMAGE Docker image..."
if [ "$REFRESH" = true ]; then
    log_info "Refreshing cfs-interop image (--no-cache)..."
    docker build --no-cache -t "$CFS_IMAGE" -f "$SCRIPT_DIR/docker/Dockerfile" "$SCRIPT_DIR"
elif ! docker image inspect "$CFS_IMAGE" &>/dev/null; then
    log_info "Building $CFS_IMAGE Docker image (this may take a while)..."
    docker build -t "$CFS_IMAGE" -f "$SCRIPT_DIR/docker/Dockerfile" "$SCRIPT_DIR"
else
    log_info "Using existing $CFS_IMAGE image"
fi

# =============================================================================
# TEST 1: cFS as server with SB echo, Hardy pings it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 1: cFS server with SB echo, Hardy pings via STCP"
echo "============================================================"

log_step "Starting cFS BPNode daemon with STCP..."

docker rm -f cfs-interop-test 2>/dev/null || true

CFS_CONTAINER=$(docker run -d \
    --name cfs-interop-test \
    --network host \
    --privileged \
    "$CFS_IMAGE")

log_info "Started cFS container: ${CFS_CONTAINER:0:12}"

# TEST 1 runs only if cFS comes up. The `for … in 1` block lets any peer-side
# setup step `break` out recording FAIL, so a cFS failure falls through to the
# summary instead of aborting the whole script.
TEST1_RESULT="FAIL"
for _ in 1; do
    # Wait until cFS is accepting STCP connections (see lib/wait.sh).
    # start_cfs sends setup/start commands internally.
    log_info "Waiting for cFS to accept connections on port $CFS_STCP_PORT..."
    if ! wait_for_port 127.0.0.1 "$CFS_STCP_PORT" 30 "$CFS_CONTAINER"; then
        log_error "cFS did not become ready on port $CFS_STCP_PORT; skipping TEST 1"
        docker logs "$CFS_CONTAINER" 2>&1 | tail -50
        break
    fi
    log_info "cFS is accepting connections on port $CFS_STCP_PORT"

    # Create STCP CLA config for Hardy's mtcp-cla binary
    cat > "$TEST_DIR/stcp_cla.toml" << EOF
bpa-address = "http://[::1]:$HARDY_GRPC_PORT"
cla-name = "cl0"
framing = "stcp"
log-level = "warn"
address = "0.0.0.0:$HARDY_STCP_PORT"
peer = "127.0.0.1:$CFS_STCP_PORT"
peer-node = "ipn:$CFS_NODE_NUM.0"
max-bundle-size = 65536
EOF

    # Hardy pings cFS echo service at ipn:CFS_NODE.7
    log_step "Hardy pinging cFS echo service at ipn:$CFS_NODE_NUM.7 via STCP..."
    echo ""

    # Exit codes: 0=success (replies received), 1=no replies (100% loss), 2=error
    PING_OUTPUT=$(timeout $((PING_COUNT * 2 + 10))s "$BP_BIN" ping "ipn:$CFS_NODE_NUM.7" \
        --cla "$MTCP_CLA_BIN" \
        --cla-args "--config $TEST_DIR/stcp_cla.toml" \
        --grpc-listen "[::1]:$HARDY_GRPC_PORT" \
        --source "ipn:$HARDY_NODE_NUM.$HARDY_SERVICE_NUM" \
        --count "$PING_COUNT" \
        2>&1) && EXIT_CODE=0 || EXIT_CODE=$?

    echo "$PING_OUTPUT"
    echo ""

    # Extract received count from "N bundles transmitted, M received" line
    STATS_LINE=$(echo "$PING_OUTPUT" | grep -E '[0-9]+ (bundles )?transmitted' | head -1)
    TRANSMITTED=$(echo "$STATS_LINE" | sed -E 's/^([0-9]+).*/\1/')
    RECEIVED=$(echo "$STATS_LINE" | sed -E 's/.*, ([0-9]+) received.*/\1/')

    if [ $EXIT_CODE -eq 0 ] && [ "$RECEIVED" = "$TRANSMITTED" ] && [ -n "$RECEIVED" ]; then
        log_info "TEST 1 PASSED: Hardy successfully pinged cFS ($RECEIVED/$TRANSMITTED)"
        TEST1_RESULT="PASS"
    elif [ $EXIT_CODE -eq 0 ]; then
        log_error "TEST 1 FAILED: Partial loss - only $RECEIVED/$TRANSMITTED responses received"
    elif [ $EXIT_CODE -eq 1 ]; then
        log_error "TEST 1 FAILED: No echo responses received (100% loss)"
    else
        log_error "TEST 1 FAILED: Error during ping (exit code $EXIT_CODE)"
    fi
done

# Show cFS container logs for diagnostics
CFS_LOGS=$(docker logs "$CFS_CONTAINER" 2>&1)
log_info "cFS container logs:"
echo "$CFS_LOGS" | grep -i 'BPNODE\|STCP\|contact\|application\|Error\|listen\|accept\|connect\|Setup\|complete bundle\|EchoApp\|echo_app\|ECHO_APP\|Syslog' | grep -v 'Child Task' | tail -30
echo ""

# =============================================================================
# TEST 2: cFS pings Hardy — cFS originates, Hardy reflects, cFS counts.
# ping_app (loaded via TEST_MODE=ping) injects N ADUs that Channel 0 wraps
# into bundles to Hardy's echo service (ipn:1.128). Hardy reflects each back
# to ipn:100.7; Channel 0 delivers them to ping_app, which counts them and
# prints the tally on a REPORT command. The reverse mirror of TEST 1, with
# no UDP telemetry hop.
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 2: cFS pings Hardy via ping_app (cFS originates, Hardy reflects)"
echo "============================================================"

# Start Hardy bpa-server with echo service and gRPC for standalone CLA
cat > "$TEST_DIR/hardy_config.toml" << EOF
log-level = "info"
status-reports = true
node-ids = "ipn:$HARDY_NODE_NUM.0"

[built-in-services]
# Hardy's echo just reflects, so it only needs the service number cFS actually
# sends to: cFS Channel 0's DestEID is hardcoded to ipn:$HARDY_NODE_NUM.$HARDY_SERVICE_NUM
# (see docker/Dockerfile). 7/8 were spurious — cFS never targets ipn:1.7 or ipn:1.8.
echo = [$HARDY_SERVICE_NUM]

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

# TEST 2 records FAIL by default; a peer-side cFS failure below falls through to
# the summary rather than aborting. Hardy/MTCP harness-start failures still set
# FAIL explicitly in their branches.
TEST2_RESULT="FAIL"

log_step "Starting Hardy BPA server..."
"$BPA_BIN" -c "$TEST_DIR/hardy_config.toml" &
HARDY_PID=$!
sleep 2

if ! kill -0 "$HARDY_PID" 2>/dev/null; then
    log_error "Hardy BPA server failed to start"
    TEST2_RESULT="FAIL"
else
    log_info "Hardy BPA server started with PID $HARDY_PID"

    # Start MTCP CLA in STCP mode, listening for cFS outbound connections
    cat > "$TEST_DIR/cla_server.toml" << EOF
bpa-address = "http://[::1]:$HARDY_GRPC_PORT"
cla-name = "cl0"
framing = "stcp"
address = "0.0.0.0:$HARDY_STCP_PORT"
peer = "127.0.0.1:$CFS_STCP_PORT"
peer-node = "ipn:$CFS_NODE_NUM.0"
EOF

    log_step "Starting MTCP CLA (STCP mode) on port $HARDY_STCP_PORT..."
    "$MTCP_CLA_BIN" --config "$TEST_DIR/cla_server.toml" &
    MTCP_PID=$!
    sleep 2

    if ! kill -0 "$MTCP_PID" 2>/dev/null; then
        log_error "MTCP CLA failed to start"
        TEST2_RESULT="FAIL"
    else
        log_info "MTCP CLA started with PID $MTCP_PID"

        # Start cFS in ping mode — ping_app originates, Hardy reflects.
        # Once contacts start, CLA Out will connect to Hardy's STCP port.
        docker rm -f cfs-interop-test 2>/dev/null || true

        CFS_CONTAINER=$(docker run -d \
            --name cfs-interop-test \
            --network host \
            --privileged \
            -e TEST_MODE=ping \
            "$CFS_IMAGE")

        log_info "Started cFS container: ${CFS_CONTAINER:0:12}"

        # Wait until cFS is accepting STCP connections. Used softly: a failure
        # here is a peer-side problem, so warn and continue rather than exit.
        log_info "Waiting for cFS to accept connections on port $CFS_STCP_PORT..."
        if wait_for_port 127.0.0.1 "$CFS_STCP_PORT" 30 "$CFS_CONTAINER"; then
            log_info "cFS is accepting connections on port $CFS_STCP_PORT"
        else
            log_warn "cFS not confirmed on port $CFS_STCP_PORT; continuing"
        fi

        if ! docker ps -q -f "id=$CFS_CONTAINER" | grep -q .; then
            log_error "cFS failed to start"
            TEST2_RESULT="FAIL"
        else
            # Give start_cfs a moment to finish contact setup (CLA Out to Hardy)
            sleep 3

            # Drive ping_app over ci_lab (UDP 1234): START a burst of N, wait
            # for the round-trips, then REPORT the tally. cmd_send is the
            # v7.0.1 command sender (cmdUtil before the rename); ci_lab forwards
            # the CCSDS packet to ping_app by its MID (0x18A1).
            CMD_PRE='CMD=$(command -v cmd_send || command -v cmdUtil); "$CMD" --port 1234 --endian=LE --pktid=0x18A1'

            log_step "Commanding ping_app to ping Hardy x$PING_COUNT..."
            docker exec "$CFS_CONTAINER" sh -c \
                "$CMD_PRE --cmdcode=0 --uint32=$PING_COUNT" >/dev/null 2>&1 || true

            # Allow the burst (~10ms/send) plus reflections to complete
            sleep $((PING_COUNT / 20 + 5))

            log_step "Requesting ping_app result..."
            docker exec "$CFS_CONTAINER" sh -c \
                "$CMD_PRE --cmdcode=1" >/dev/null 2>&1 || true
            sleep 1

            # Read the authoritative tally ping_app logged to the cFS console
            CFS_LOGS2=$(docker logs "$CFS_CONTAINER" 2>&1)
            RESULT_LINE=$(echo "$CFS_LOGS2" | grep -E 'PINGAPP: RESULT' | tail -1)
            PING_SENT=$(echo "$RESULT_LINE" | sed -nE 's/.*sent=([0-9]+).*/\1/p')
            PING_RECV=$(echo "$RESULT_LINE" | sed -nE 's/.*received=([0-9]+).*/\1/p')

            [ -n "$RESULT_LINE" ] && echo "  $RESULT_LINE"
            log_info "ping_app reported sent=${PING_SENT:-?} received=${PING_RECV:-?}"

            if [ -n "$PING_RECV" ] && [ "$PING_RECV" = "$PING_SENT" ] && [ "$PING_SENT" = "$PING_COUNT" ]; then
                log_info "TEST 2 PASSED: cFS pinged Hardy ($PING_RECV/$PING_COUNT reflected)"
                TEST2_RESULT="PASS"
            else
                log_error "TEST 2 FAILED: sent=${PING_SENT:-0} received=${PING_RECV:-0} (expected $PING_COUNT)"
                TEST2_RESULT="FAIL"
            fi
        fi
    fi
fi

# Stop cFS
if [ -n "$CFS_CONTAINER" ]; then
    docker stop -t 2 "$CFS_CONTAINER" 2>/dev/null || true
    docker rm -f "$CFS_CONTAINER" 2>/dev/null || true
    CFS_CONTAINER=""
fi

# =============================================================================
# Summary
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST SUMMARY"
echo "============================================================"
echo ""
echo "  TEST 1 (Hardy pings cFS via STCP):     $TEST1_RESULT"
echo "  TEST 2 (cFS pings Hardy via ping_app): ${TEST2_RESULT:-SKIP}"
echo ""

if [ "$TEST1_RESULT" = "PASS" ] && [ "${TEST2_RESULT:-FAIL}" = "PASS" ]; then
    log_info "All interoperability tests passed"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
