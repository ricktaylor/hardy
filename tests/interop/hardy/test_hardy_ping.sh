#!/bin/bash
# Interoperability test: Hardy <-> Hardy ping/echo
#
# This script tests bidirectional ping/echo between two Hardy BPA servers:
#   1. Node 1 pings Node 2's echo service
#   2. Node 2 pings Node 1's echo service
#
# Prerequisites:
#   - Hardy tools and bpa-server built
#
# Usage:
#   ./tests/interop/hardy/test_hardy_ping.sh [--skip-build]
#
# Options:
#   --skip-build   Skip building Hardy binaries

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# Configuration
NODE1_NUM=1
NODE2_NUM=2
NODE1_PORT=4560
NODE2_PORT=4561
PING_COUNT=5
# Ping source service number (fixed for routing)
PING_SERVICE=12345

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
while [[ $# -gt 0 ]]; do
    case $1 in
        --skip-build)
            SKIP_BUILD=true
            shift
            ;;
        *)
            echo "Unknown option: $1"
            exit 1
            ;;
    esac
done

# Cleanup function
NODE1_PID=""
NODE2_PID=""
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

    # Stop BPA servers
    kill_process "$NODE1_PID" "hardy-node-1"
    kill_process "$NODE2_PID" "hardy-node-2"

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

if [ ! -x "$BPA_BIN" ]; then
    log_error "hardy-bpa-server binary not found at $BPA_BIN"
    exit 1
fi

# =============================================================================
# Start both Hardy BPA servers
# =============================================================================
log_step "Starting Hardy BPA servers..."

# Create Node 1 config
# Note: No static routes needed - CLA peer registration automatically adds
# wildcard patterns (e.g., ipn:1.* when peer ipn:1.0 connects)
cat > "$TEST_DIR/node1_config.toml" << EOF
log_level = "info"
status_reports = true
node_ids = "ipn:$NODE1_NUM.0"

# Echo service on IPN service 7
echo = 7

[metadata_storage]
type = "memory"

[bundle_storage]
type = "memory"

[[clas]]
name = "cl0"
type = "tcpclv4"
address = "[::]:$NODE1_PORT"
must_use_tls = false
EOF

# Create Node 2 config
cat > "$TEST_DIR/node2_config.toml" << EOF
log_level = "info"
status_reports = true
node_ids = "ipn:$NODE2_NUM.0"

# Echo service on IPN service 7
echo = 7

[metadata_storage]
type = "memory"

[bundle_storage]
type = "memory"

[[clas]]
name = "cl0"
type = "tcpclv4"
address = "[::]:$NODE2_PORT"
must_use_tls = false
EOF

# Start Node 1
log_info "Starting Node 1 (ipn:$NODE1_NUM.0) on port $NODE1_PORT..."
"$BPA_BIN" -c "$TEST_DIR/node1_config.toml" &
NODE1_PID=$!

# Start Node 2
log_info "Starting Node 2 (ipn:$NODE2_NUM.0) on port $NODE2_PORT..."
"$BPA_BIN" -c "$TEST_DIR/node2_config.toml" &
NODE2_PID=$!

# Wait for both servers to start
sleep 3

# Verify both are running
if ! kill -0 "$NODE1_PID" 2>/dev/null; then
    log_error "Node 1 BPA server failed to start"
    exit 1
fi
log_info "Node 1 started with PID $NODE1_PID"

if ! kill -0 "$NODE2_PID" 2>/dev/null; then
    log_error "Node 2 BPA server failed to start"
    exit 1
fi
log_info "Node 2 started with PID $NODE2_PID"

# =============================================================================
# TEST 1: Node 1 pings Node 2's echo service
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 1: Node 1 pings Node 2's echo service"
echo "============================================================"

log_step "Pinging Node 2's echo service at ipn:$NODE2_NUM.7 (source: ipn:$NODE1_NUM.$PING_SERVICE)..."
echo ""

# Exit codes: 0=success (replies received), 1=no replies (100% loss), 2=error
# Use && true || true pattern to prevent set -e from exiting on non-zero
"$BP_BIN" ping "ipn:$NODE2_NUM.7" "127.0.0.1:$NODE2_PORT" \
    --source "ipn:$NODE1_NUM.$PING_SERVICE" \
    --count "$PING_COUNT" \
    --no-sign \
    && EXIT_CODE=0 || EXIT_CODE=$?
if [ $EXIT_CODE -eq 0 ]; then
    log_info "TEST 1 PASSED: Successfully pinged Node 2 with echo responses"
    TEST1_RESULT="PASS"
elif [ $EXIT_CODE -eq 1 ]; then
    log_error "TEST 1 FAILED: No echo responses received (100% loss)"
    TEST1_RESULT="FAIL"
else
    log_error "TEST 1 FAILED: Error during ping (exit code $EXIT_CODE)"
    TEST1_RESULT="FAIL"
fi

# =============================================================================
# TEST 2: Node 2 pings Node 1's echo service
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 2: Node 2 pings Node 1's echo service"
echo "============================================================"

log_step "Pinging Node 1's echo service at ipn:$NODE1_NUM.7 (source: ipn:$NODE2_NUM.$PING_SERVICE)..."
echo ""

# Exit codes: 0=success (replies received), 1=no replies (100% loss), 2=error
# Use && true || true pattern to prevent set -e from exiting on non-zero
"$BP_BIN" ping "ipn:$NODE1_NUM.7" "127.0.0.1:$NODE1_PORT" \
    --source "ipn:$NODE2_NUM.$PING_SERVICE" \
    --count "$PING_COUNT" \
    --no-sign \
    && EXIT_CODE=0 || EXIT_CODE=$?
if [ $EXIT_CODE -eq 0 ]; then
    log_info "TEST 2 PASSED: Successfully pinged Node 1 with echo responses"
    TEST2_RESULT="PASS"
elif [ $EXIT_CODE -eq 1 ]; then
    log_error "TEST 2 FAILED: No echo responses received (100% loss)"
    TEST2_RESULT="FAIL"
else
    log_error "TEST 2 FAILED: Error during ping (exit code $EXIT_CODE)"
    TEST2_RESULT="FAIL"
fi

# =============================================================================
# TEST 3: Multi-hop test (Node 1 -> Node 2 as forwarder back to Node 1)
# This tests that bundles can traverse through another node
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 3: Self-ping via Node 2 (tests forwarding)"
echo "============================================================"

# For this test, we ping Node 1's echo service via Node 2
# This creates: ping tool -> Node 2 -> Node 1 echo -> Node 1 -> Node 2 -> ping tool
# Note: This requires Node 2 to know how to reach Node 1, which it doesn't in this simple config.
# Skip this for now as it needs static route configuration.
log_warn "TEST 3 SKIPPED: Multi-hop test requires static route configuration"
TEST3_RESULT="SKIP"

# =============================================================================
# Summary
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST SUMMARY"
echo "============================================================"
echo ""
echo "  TEST 1 (Node 1 pings Node 2): $TEST1_RESULT"
echo "  TEST 2 (Node 2 pings Node 1): $TEST2_RESULT"
echo "  TEST 3 (Multi-hop forwarding): $TEST3_RESULT"
echo ""

# Determine exit code
if [ "$TEST1_RESULT" = "PASS" ] && [ "$TEST2_RESULT" = "PASS" ]; then
    log_info "Hardy-to-Hardy interoperability test completed successfully"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
