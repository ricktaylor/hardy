#!/bin/bash
# End-to-end test: hardy-tvr contact scheduling
#
# Tests that hardy-tvr correctly installs and withdraws routes in the BPA
# based on contact plan schedules, and that bundles flow during contact
# windows.
#
# Architecture:
#   ┌──────────┐  gRPC   ┌───────────┐  routes  ┌──────────┐  TCPCLv4  ┌──────────┐
#   │ hardy-tvr│◄───────►│ BPA Node1 │◄────────►│ BPA Node1│◄────────►│ BPA Node2│
#   │ (sched)  │ :50051  │ (routes)  │          │ (fwd)    │  :4560   │ (echo)   │
#   └──────────┘         └───────────┘          └──────────┘          └──────────┘
#
# Tests:
#   1. Permanent route: ping succeeds immediately
#   2. Hot-reload: add a route by modifying the contact plan file
#   3. File removal: withdraw routes by deleting the contact plan file
#   4. File restore: re-add routes by recreating the contact plan file
#   5. gRPC session open: open session via grpcurl, verify response
#   6. gRPC add contacts: add contacts via session, verify route installed
#   7. gRPC session close cleanup: close session, verify routes withdrawn
#   8. gRPC duplicate session name: second session with same name rejected
#   9. gRPC missing open: send add as first message, verify rejection
#  10. gRPC session name reuse: re-open session after close succeeds
#
# Usage:
#   ./tvr/tests/test_tvr.sh [--skip-build] [--count N]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Configuration
NODE1_NUM=1
NODE2_NUM=2
NODE3_NUM=3  # phantom node — no CLA, route-only
NODE1_TCPCLV4_PORT=4560
NODE2_TCPCLV4_PORT=4561
BPA_GRPC_PORT=50051
TVR_GRPC_PORT=50052
PING_COUNT=3
PING_SERVICE=12345

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }
log_step() { echo -e "${BLUE}[STEP]${NC} $*"; }

# Parse options
SKIP_BUILD=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --skip-build) SKIP_BUILD=true; shift ;;
        --count|-c) PING_COUNT="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# PIDs for cleanup
NODE1_PID=""
NODE2_PID=""
TVR_PID=""
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
            log_warn "Force killing $name..."
            kill -9 "$pid" 2>/dev/null || true
        fi
        wait "$pid" 2>/dev/null || true
    fi
}

cleanup() {
    if [ -n "$CLEANUP_IN_PROGRESS" ]; then return; fi
    CLEANUP_IN_PROGRESS=1
    log_info "Cleaning up..."
    kill_process "$TVR_PID" "hardy-tvr"
    kill_process "$NODE1_PID" "bpa-node-1"
    kill_process "$NODE2_PID" "bpa-node-2"
    if [ -n "$TEST_DIR" ] && [ -d "$TEST_DIR" ]; then
        rm -rf "$TEST_DIR"
    fi
    log_info "Cleanup complete"
}
trap cleanup EXIT INT TERM

# Create temporary directory
TEST_DIR=$(mktemp -d)
log_info "Using test directory: $TEST_DIR"

# Build if needed
if [ "$SKIP_BUILD" = false ]; then
    log_step "Building Hardy binaries..."
    cd "$WORKSPACE_DIR"
    cargo build --release -p hardy-tools -p hardy-bpa-server -p hardy-tvr
fi

BP_BIN="$WORKSPACE_DIR/target/release/bp"
BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"
TVR_BIN="$WORKSPACE_DIR/target/release/hardy-tvr"

for bin in "$BP_BIN" "$BPA_BIN" "$TVR_BIN"; do
    if [ ! -x "$bin" ]; then
        log_error "Binary not found: $bin"
        exit 1
    fi
done

# grpcurl configuration for TVR gRPC session tests
GRPCURL_ARGS="-plaintext -import-path $WORKSPACE_DIR/tvr -import-path $WORKSPACE_DIR/proto -proto tvr.proto"
TVR_ADDR="[::1]:$TVR_GRPC_PORT"

# Helper: invoke grpcurl against the TVR service with stdin data
# Usage: echo '...' | tvr_grpcurl
tvr_grpcurl() {
    grpcurl $GRPCURL_ARGS -d @ "$TVR_ADDR" tvr.Tvr/Session
}

# Helper: run a ping and check result
do_ping() {
    local dest=$1
    local peer=$2
    local expect=$3  # "pass" or "fail"
    local label=$4

    local output exit_code
    output=$("$BP_BIN" ping "$dest" "$peer" \
        --source "ipn:$NODE1_NUM.$PING_SERVICE" \
        --count "$PING_COUNT" \
        --no-sign \
        2>&1) && exit_code=0 || exit_code=$?

    if [ "$expect" = "pass" ]; then
        if [ $exit_code -eq 0 ]; then
            log_info "$label: PASSED"
            return 0
        else
            log_error "$label: FAILED (expected success, got exit $exit_code)"
            echo "$output"
            return 1
        fi
    else
        if [ $exit_code -ne 0 ]; then
            log_info "$label: PASSED (correctly failed)"
            return 0
        else
            log_error "$label: FAILED (expected failure, but ping succeeded)"
            echo "$output"
            return 1
        fi
    fi
}

# =============================================================================
# Start BPA nodes
# =============================================================================
log_step "Starting BPA servers..."

# Node 1: has gRPC enabled (for hardy-tvr), TCPCLv4 for peering
cat > "$TEST_DIR/node1.toml" << EOF
log-level = "info"
node-ids = "ipn:$NODE1_NUM.0"

[built-in-services]
echo = [7]

[storage.metadata]
type = "memory"

[storage.bundle]
type = "memory"

[grpc]
address = "[::1]:$BPA_GRPC_PORT"
services = ["routing"]

[[clas]]
name = "cl0"
type = "tcpclv4"
address = "[::]:$NODE1_TCPCLV4_PORT"
EOF

# Node 2: echo service, TCPCLv4
cat > "$TEST_DIR/node2.toml" << EOF
log-level = "info"
node-ids = "ipn:$NODE2_NUM.0"

[built-in-services]
echo = [7]

[storage.metadata]
type = "memory"

[storage.bundle]
type = "memory"

[[clas]]
name = "cl0"
type = "tcpclv4"
address = "[::]:$NODE2_TCPCLV4_PORT"
EOF

"$BPA_BIN" -c "$TEST_DIR/node1.toml" &
NODE1_PID=$!

"$BPA_BIN" -c "$TEST_DIR/node2.toml" &
NODE2_PID=$!

sleep 2

for pid_var in NODE1_PID NODE2_PID; do
    pid=${!pid_var}
    if ! kill -0 "$pid" 2>/dev/null; then
        log_error "BPA server failed to start ($pid_var)"
        exit 1
    fi
done
log_info "BPA servers started"

# =============================================================================
# TEST 1: Permanent route via hardy-tvr
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 1: Permanent route — ping succeeds"
echo "============================================================"

# Create contact plan with a permanent route to Node 2
cat > "$TEST_DIR/contacts" << EOF
# Route to Node 2 via TCPCLv4
ipn:$NODE2_NUM.*.* via ipn:$NODE2_NUM.1.0 priority 10
EOF

# Start hardy-tvr
cat > "$TEST_DIR/tvr.toml" << EOF
bpa-address = "http://[::1]:$BPA_GRPC_PORT"
contact-plan = "$TEST_DIR/contacts"
grpc-listen = "[::1]:$TVR_GRPC_PORT"
log-level = "info"
EOF

"$TVR_BIN" -c "$TEST_DIR/tvr.toml" &
TVR_PID=$!

sleep 2

if ! kill -0 "$TVR_PID" 2>/dev/null; then
    log_error "hardy-tvr failed to start"
    exit 1
fi
log_info "hardy-tvr started with PID $TVR_PID"

# Ping Node 2 — should succeed (permanent route installed)
do_ping "ipn:$NODE2_NUM.7" "127.0.0.1:$NODE2_TCPCLV4_PORT" pass "Permanent route ping"
TEST1=$?

# =============================================================================
# TEST 2: Hot-reload — add a route to phantom node
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 2: Hot-reload — add route to phantom node"
echo "============================================================"

# Add a route to a phantom node (no CLA peer, no echo service).
# We can't ping it, but we verify the route is installed by checking
# that the BPA attempts to forward (bundle enters ForwardPending/Waiting).
# For this test, we just verify the file reload succeeds and TVR logs
# the addition.
cat > "$TEST_DIR/contacts" << EOF
ipn:$NODE2_NUM.*.* via ipn:$NODE2_NUM.1.0 priority 10
ipn:$NODE3_NUM.*.* via ipn:$NODE3_NUM.1.0 priority 20
EOF

# Wait for debounce + reload
sleep 3

# Original route should still work
do_ping "ipn:$NODE2_NUM.7" "127.0.0.1:$NODE2_TCPCLV4_PORT" pass "After hot-reload ping"
TEST2=$?

# =============================================================================
# TEST 3: File removal — withdraw routes, phantom node unreachable
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 3: File removal — withdraw phantom node route"
echo "============================================================"

# Replace with only the phantom node route (no Node 2 route).
# Then delete the file entirely to withdraw everything.
cat > "$TEST_DIR/contacts" << EOF
ipn:$NODE3_NUM.*.* via ipn:$NODE3_NUM.1.0 priority 20
EOF

sleep 3

# Delete the file — all TVR routes withdrawn
rm -f "$TEST_DIR/contacts"

sleep 3

# Ping the phantom node — should fail (no route, no CLA peer)
do_ping "ipn:$NODE3_NUM.7" "127.0.0.1:$NODE2_TCPCLV4_PORT" fail "Phantom node after file removal"
TEST3=$?

# =============================================================================
# TEST 4: File restore — re-add routes
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 4: File restore — re-add Node 2 route"
echo "============================================================"

# Recreate the contact plan with the real route
cat > "$TEST_DIR/contacts" << EOF
ipn:$NODE2_NUM.*.* via ipn:$NODE2_NUM.1.0 priority 10
EOF

# Wait for debounce + reload
sleep 3

# Ping Node 2 — should work again
do_ping "ipn:$NODE2_NUM.7" "127.0.0.1:$NODE2_TCPCLV4_PORT" pass "After file restore ping"
TEST4=$?

# =============================================================================
# TEST 5: gRPC session open (TVR-01)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 5: gRPC session open"
echo "============================================================"

# Open a session via grpcurl and verify we get an OpenSessionResponse
output=$(echo '{"msg_id": 1, "open": {"name": "test-open", "default_priority": 100}}' \
    | tvr_grpcurl 2>&1) && exit_code=0 || exit_code=$?

if echo "$output" | grep -q '"open"'; then
    log_info "gRPC session open: PASSED"
    TEST5=0
else
    log_error "gRPC session open: FAILED"
    echo "$output"
    TEST5=1
fi

# =============================================================================
# TEST 6: gRPC add contacts + route verification (TVR-05, TVR-09)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 6: gRPC add contacts via session"
echo "============================================================"

# First, remove the file-based contacts so only gRPC routes are active
rm -f "$TEST_DIR/contacts"
sleep 3

# Open a session in background using a FIFO to keep the stream alive
ADD_FIFO="$TEST_DIR/add_fifo"
mkfifo "$ADD_FIFO"
tvr_grpcurl < "$ADD_FIFO" > "$TEST_DIR/grpc_output.json" 2>&1 &
GRPC_PID=$!
exec 4>"$ADD_FIFO"
echo '{"msg_id": 1, "open": {"name": "route-test", "default_priority": 100}}' >&4
echo '{"msg_id": 2, "add": {"contacts": [{"eid_pattern": "ipn:'"$NODE2_NUM"'.*.*", "via": "ipn:'"$NODE2_NUM"'.1.0", "priority": 10}]}}' >&4

sleep 2

# Verify the add response contains added count
if grep -q '"added":' "$TEST_DIR/grpc_output.json" 2>/dev/null; then
    log_info "gRPC add contacts: PASSED"
    TEST6=0
else
    log_error "gRPC add contacts: FAILED (no add response)"
    cat "$TEST_DIR/grpc_output.json" 2>/dev/null
    TEST6=1
fi

# =============================================================================
# TEST 7: gRPC session close cleanup (TVR-09)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 7: gRPC session close — routes withdrawn"
echo "============================================================"

# Close fd 4 to close the FIFO, ending the grpcurl stream
exec 4>&-
if [ -n "$GRPC_PID" ] && kill -0 "$GRPC_PID" 2>/dev/null; then
    kill "$GRPC_PID" 2>/dev/null || true
    wait "$GRPC_PID" 2>/dev/null || true
fi
rm -f "$ADD_FIFO"

# Wait for TVR to process the stream close and withdraw routes
sleep 2

# Verify cleanup: open a new session and re-add the same route.
# If cleanup worked, the route was withdrawn and re-adding it should
# produce "active": 1 (newly installed). If cleanup failed, the route
# would still be in the BPA from the previous session.
output=$(cat << 'EOF' | tvr_grpcurl 2>&1
{"msg_id": 1, "open": {"name": "cleanup-check", "default_priority": 100}}
{"msg_id": 2, "add": {"contacts": [{"eid_pattern": "ipn:2.*.*", "via": "ipn:2.1.0", "priority": 10}]}}
EOF
)

if echo "$output" | grep -q '"active"'; then
    log_info "gRPC session close cleanup: PASSED"
    TEST7=0
else
    log_error "gRPC session close cleanup: FAILED"
    echo "$output"
    TEST7=1
fi

# =============================================================================
# TEST 8: gRPC duplicate session name (TVR-02)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 8: gRPC duplicate session name rejected"
echo "============================================================"

# Start a session in background using a FIFO to keep stdin open.
# Open fd 3 as a persistent writer so grpcurl's stdin stays open
# after we write the open message.
DUP_FIFO="$TEST_DIR/dup_fifo"
mkfifo "$DUP_FIFO"
tvr_grpcurl < "$DUP_FIFO" > /dev/null 2>&1 &
DUP_PID1=$!
exec 3>"$DUP_FIFO"
echo '{"msg_id": 1, "open": {"name": "dup-test", "default_priority": 100}}' >&3

sleep 1

# Try to open a second session with the same name
output=$(echo '{"msg_id": 1, "open": {"name": "dup-test", "default_priority": 100}}' \
    | tvr_grpcurl 2>&1) && exit_code=0 || exit_code=$?

# Clean up first session — close fd 3 to close the FIFO, then wait
exec 3>&-
kill "$DUP_PID1" 2>/dev/null || true
wait "$DUP_PID1" 2>/dev/null || true
rm -f "$DUP_FIFO"

if echo "$output" | grep -qi "already"; then
    log_info "gRPC duplicate session name: PASSED"
    TEST8=0
else
    log_error "gRPC duplicate session name: FAILED"
    echo "$output"
    TEST8=1
fi

# =============================================================================
# TEST 9: gRPC missing open (TVR-03)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 9: gRPC missing open — rejected"
echo "============================================================"

# Send an add as the first message (no open)
output=$(echo '{"msg_id": 1, "add": {"contacts": [{"eid_pattern": "ipn:2.*.*", "via": "ipn:2.1.0"}]}}' \
    | tvr_grpcurl 2>&1) && exit_code=0 || exit_code=$?

if echo "$output" | grep -qi "OpenSession\|INVALID_ARGUMENT\|InvalidArgument"; then
    log_info "gRPC missing open: PASSED"
    TEST9=0
else
    log_error "gRPC missing open: FAILED"
    echo "$output"
    TEST9=1
fi

# =============================================================================
# TEST 10: gRPC session name reuse after close (TVR-12)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 10: gRPC session name reuse after close"
echo "============================================================"

# Open and close a session
echo '{"msg_id": 1, "open": {"name": "reuse-test", "default_priority": 100}}' \
    | tvr_grpcurl > /dev/null 2>&1

sleep 1

# Re-open with the same name — should succeed
output=$(echo '{"msg_id": 1, "open": {"name": "reuse-test", "default_priority": 100}}' \
    | tvr_grpcurl 2>&1) && exit_code=0 || exit_code=$?

if echo "$output" | grep -q '"open"'; then
    log_info "gRPC session name reuse: PASSED"
    TEST10=0
else
    log_error "gRPC session name reuse: FAILED"
    echo "$output"
    TEST10=1
fi

# =============================================================================
# Summary
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST SUMMARY"
echo "============================================================"
echo ""

PASS=0
FAIL=0

for t in TEST1 TEST2 TEST3 TEST4 TEST5 TEST6 TEST7 TEST8 TEST9 TEST10; do
    val=${!t}
    case $t in
        TEST1)  desc="Permanent route" ;;
        TEST2)  desc="Hot-reload (add)" ;;
        TEST3)  desc="File removal" ;;
        TEST4)  desc="File restore" ;;
        TEST5)  desc="gRPC session open" ;;
        TEST6)  desc="gRPC add contacts + route" ;;
        TEST7)  desc="gRPC session close cleanup" ;;
        TEST8)  desc="gRPC duplicate session name" ;;
        TEST9)  desc="gRPC missing open" ;;
        TEST10) desc="gRPC session name reuse" ;;
    esac
    if [ "$val" -eq 0 ]; then
        echo "  $desc: PASS"
        PASS=$((PASS + 1))
    else
        echo "  $desc: FAIL"
        FAIL=$((FAIL + 1))
    fi
done

echo ""
echo "  $PASS passed, $FAIL failed"
echo ""

if [ "$FAIL" -eq 0 ]; then
    log_info "All TVR tests passed"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
