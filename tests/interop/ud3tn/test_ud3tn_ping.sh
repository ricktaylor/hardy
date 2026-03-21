#!/bin/bash
# Interoperability test: Hardy <-> ud3tn ping/echo via MTCP
#
# This script tests bidirectional ping/echo between Hardy and ud3tn:
#   1. ud3tn as server with echo agent, Hardy pings it via MTCP
#   2. Hardy as server with echo service, ud3tn pings it via MTCP
#
# Prerequisites:
#   - Docker installed (for ud3tn container)
#   - Hardy tools and bpa-server built (with dynamic-plugins feature)
#   - MTCP/STCP CLA plugin built (tests/interop/mtcp/cla/)
#   - ud3tn Docker image built (ud3tn-interop)
#
# Usage:
#   ./tests/interop/ud3tn/test_ud3tn_ping.sh [--skip-build] [--no-docker]
#
# Options:
#   --skip-build   Skip building Hardy and CLA plugin binaries
#   --no-docker    Use local ud3tn binaries instead of Docker

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INTEROP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
MTCP_CLA_DIR="$INTEROP_DIR/mtcp/cla"

# Configuration
HARDY_NODE_NUM=1
UD3TN_NODE_NUM=2
UD3TN_MTCP_PORT=4556
HARDY_MTCP_PORT=4557
# ud3tn AAP2 port for agent registration
UD3TN_AAP2_PORT=4243
UD3TN_IMAGE="ud3tn-interop"
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
UD3TN_CONTAINER=""
HARDY_PID=""
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

    if [ -n "$UD3TN_CONTAINER" ]; then
        docker stop -t 2 "$UD3TN_CONTAINER" 2>/dev/null || true
        docker rm -f "$UD3TN_CONTAINER" 2>/dev/null || true
    fi
    docker rm -f ud3tn-interop-test 2>/dev/null || true

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

# Build Hardy tools, server, and MTCP CLA plugin if needed
if [ "$SKIP_BUILD" = false ]; then
    log_step "Building Hardy tools and bpa-server..."
    cd "$WORKSPACE_DIR"
    cargo build --release -p hardy-tools -p hardy-bpa-server --features dynamic-plugins

    log_step "Building MTCP/STCP CLA plugin..."
    cd "$MTCP_CLA_DIR"
    cargo build --release
fi

BP_BIN="$WORKSPACE_DIR/target/release/bp"
BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"
CLA_PLUGIN="$MTCP_CLA_DIR/target/release/libhardy_mtcp_cla.so"

if [ ! -x "$BP_BIN" ]; then
    log_error "bp binary not found at $BP_BIN"
    exit 1
fi

if [ ! -f "$CLA_PLUGIN" ]; then
    log_error "MTCP CLA plugin not found at $CLA_PLUGIN"
    log_error "Build it with: cd $MTCP_CLA_DIR && cargo build --release"
    exit 1
fi

# Build or check for ud3tn
if [ "$USE_DOCKER" = true ]; then
    log_step "Checking for ud3tn-interop Docker image..."
    if ! docker image inspect "$UD3TN_IMAGE" &>/dev/null; then
        log_info "Building ud3tn-interop Docker image..."
        docker build -t "$UD3TN_IMAGE" "$SCRIPT_DIR/docker"
    else
        log_info "Using existing ud3tn-interop image"
    fi
else
    if ! command -v ud3tn &> /dev/null; then
        log_error "ud3tn not found in PATH"
        log_error "Install ud3tn or use Docker mode"
        exit 1
    fi
    log_info "Found ud3tn at: $(which ud3tn)"
fi

# =============================================================================
# TEST 1: ud3tn as server with echo agent, Hardy pings it via MTCP
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 1: ud3tn server with echo, Hardy pings via MTCP"
echo "============================================================"

log_step "Starting ud3tn daemon with MTCP CL..."

if [ "$USE_DOCKER" = true ]; then
    docker rm -f ud3tn-interop-test 2>/dev/null || true

    # -e: EID, -c: CLA options, -A/-P: AAP2 TCP host/port, -R: allow remote config
    UD3TN_CONTAINER=$(docker run -d \
        --name ud3tn-interop-test \
        --network host \
        "$UD3TN_IMAGE" \
        -e "ipn:$UD3TN_NODE_NUM.0" \
        -c "mtcp:0.0.0.0,$UD3TN_MTCP_PORT" \
        -b 7 \
        -A 0.0.0.0 -P "$UD3TN_AAP2_PORT" \
        -R)

    log_info "Started ud3tn container: ${UD3TN_CONTAINER:0:12}"

    log_info "Waiting for ud3tn to initialize..."
    sleep 3

    if ! docker ps -q -f "id=$UD3TN_CONTAINER" | grep -q .; then
        log_error "ud3tn container exited unexpectedly. Logs:"
        docker logs "$UD3TN_CONTAINER" 2>&1 | tail -50
        docker rm "$UD3TN_CONTAINER" 2>/dev/null || true
        exit 1
    fi

    # Start echo agent via AAP inside the container
    # aap_echo registers on a service endpoint and echoes bundles back
    log_step "Starting echo agent on ipn:$UD3TN_NODE_NUM.7..."
    docker exec -d "$UD3TN_CONTAINER" \
        python3 -m ud3tn_utils.aap.bin.aap_echo \
        --agentid 7 \
        --tcp 127.0.0.1 4242 \
        2>/dev/null || {
        # Fallback: try AAP2-based echo if AAP1 isn't available
        docker exec -d "$UD3TN_CONTAINER" \
            python3 -m ud3tn_utils.aap2.bin.aap2_receive \
            --agentid 7 \
            --tcp 127.0.0.1 "$UD3TN_AAP2_PORT" \
            2>/dev/null || log_warn "Could not start echo agent"
    }

    sleep 2
else
    log_error "Native ud3tn mode not yet implemented - use Docker mode"
    exit 1
fi

# Configure ud3tn contact to Hardy (so it knows how to route responses back)
log_step "Configuring ud3tn contact to Hardy node..."
docker exec "$UD3TN_CONTAINER" \
    python3 -m ud3tn_utils.aap2.bin.aap2_configure_link \
    --tcp 127.0.0.1 "$UD3TN_AAP2_PORT" \
    "ipn:$HARDY_NODE_NUM.0" \
    "mtcp:127.0.0.1:$HARDY_MTCP_PORT" \
    2>/dev/null || log_warn "Could not configure contact (may not be needed)"

sleep 1

# Hardy pings ud3tn echo service via MTCP using the CLA plugin
# Note: Hardy also needs to listen for MTCP responses from ud3tn
log_step "Hardy pinging ud3tn echo service at ipn:$UD3TN_NODE_NUM.7 via MTCP..."
echo ""

PING_OUTPUT=$("$BP_BIN" ping "ipn:$UD3TN_NODE_NUM.7" \
    --cla "$CLA_PLUGIN" \
    --cla-config "{\"framing\":\"mtcp\",\"peer\":\"127.0.0.1:$UD3TN_MTCP_PORT\",\"peer-node\":\"ipn:$UD3TN_NODE_NUM.0\",\"address\":\"[::]:$HARDY_MTCP_PORT\"}" \
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
        log_info "TEST 1 PASSED: Hardy successfully pinged ud3tn ($RECEIVED/$TRANSMITTED)"
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

# Stop ud3tn for test 2
log_info "Stopping ud3tn..."
if [ "$USE_DOCKER" = true ]; then
    docker stop "$UD3TN_CONTAINER" 2>/dev/null || true
    docker rm -f "$UD3TN_CONTAINER" 2>/dev/null || true
    UD3TN_CONTAINER=""
fi

sleep 1

# =============================================================================
# TEST 2: Hardy as server with echo, ud3tn pings it via MTCP
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 2: Hardy server with echo, ud3tn pings via MTCP"
echo "============================================================"

# Create Hardy config for server mode with MTCP CLA plugin
cat > "$TEST_DIR/hardy_config.toml" << EOF
log-level = "info"
status-reports = true
node-ids = "ipn:$HARDY_NODE_NUM.0"
plugin-dir = "$(dirname "$CLA_PLUGIN")"

[built-in-services]
echo = [7]

[storage.metadata]
type = "memory"

[storage.bundle]
type = "memory"

[rfc9171-validity]
primary-block-integrity = false

[[clas]]
name = "mtcp0"
type = "hardy_mtcp_cla"
framing = "mtcp"
address = "[::]:$HARDY_MTCP_PORT"
EOF

log_step "Starting Hardy BPA server with MTCP CLA plugin..."
"$BPA_BIN" -c "$TEST_DIR/hardy_config.toml" &
HARDY_PID=$!

sleep 3

if ! kill -0 "$HARDY_PID" 2>/dev/null; then
    log_error "Hardy BPA server failed to start"
    exit 1
fi
log_info "Hardy BPA server started with PID $HARDY_PID"

# Start ud3tn to ping Hardy
log_step "Starting ud3tn to ping Hardy..."

if [ "$USE_DOCKER" = true ]; then
    docker rm -f ud3tn-interop-test 2>/dev/null || true

    UD3TN_CONTAINER=$(docker run -d \
        --name ud3tn-interop-test \
        --network host \
        "$UD3TN_IMAGE" \
        -e "ipn:$UD3TN_NODE_NUM.0" \
        -c "mtcp:0.0.0.0,$UD3TN_MTCP_PORT" \
        -b 7 \
        -A 0.0.0.0 -P "$UD3TN_AAP2_PORT" \
        -R)

    log_info "Started ud3tn container: ${UD3TN_CONTAINER:0:12}"
    sleep 3

    if ! docker ps -q -f "id=$UD3TN_CONTAINER" | grep -q .; then
        log_error "ud3tn container exited unexpectedly. Logs:"
        docker logs "$UD3TN_CONTAINER" 2>&1 | tail -20
        docker rm "$UD3TN_CONTAINER" 2>/dev/null || true
        TEST2_RESULT="FAIL"
    else
        # Configure contact to Hardy
        log_step "Configuring ud3tn contact to Hardy..."
        docker exec "$UD3TN_CONTAINER" \
            python3 -m ud3tn_utils.aap2.bin.aap2_configure_link \
            --tcp 127.0.0.1 "$UD3TN_AAP2_PORT" \
            "ipn:$HARDY_NODE_NUM.0" \
            "mtcp:127.0.0.1:$HARDY_MTCP_PORT" \
            2>/dev/null || log_warn "Could not configure contact"

        sleep 2

        # Run ping from ud3tn container using aap2_ping
        log_step "ud3tn pinging Hardy echo service at ipn:$HARDY_NODE_NUM.7..."
        PING_TIMEOUT=$((PING_COUNT * 2 + 10))
        PING_OUTPUT=$(timeout "${PING_TIMEOUT}s" docker exec "$UD3TN_CONTAINER" \
            python3 -m ud3tn_utils.aap2.bin.aap2_ping \
            --tcp 127.0.0.1 "$UD3TN_AAP2_PORT" \
            --agentid 128 \
            --count "$PING_COUNT" \
            "ipn:$HARDY_NODE_NUM.7" \
            2>&1) || true

        echo "$PING_OUTPUT"
        echo ""

        # aap2_ping reports round-trip times; count lines with time info
        RESPONSE_COUNT=$(echo "$PING_OUTPUT" | grep -ci "time\|reply\|response\|ms" || echo "0")

        if [ "$RESPONSE_COUNT" -ge "$PING_COUNT" ]; then
            log_info "TEST 2 PASSED: ud3tn received responses from Hardy"
            TEST2_RESULT="PASS"
        elif [ "$RESPONSE_COUNT" -ge 1 ]; then
            log_error "TEST 2 FAILED: Partial loss - only $RESPONSE_COUNT responses detected"
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
echo "  TEST 1 (Hardy pings ud3tn via MTCP): $TEST1_RESULT"
echo "  TEST 2 (ud3tn pings Hardy via MTCP): $TEST2_RESULT"
echo ""

if [ "$TEST1_RESULT" = "PASS" ] && [ "$TEST2_RESULT" = "PASS" ]; then
    log_info "All ud3tn interoperability tests passed"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
