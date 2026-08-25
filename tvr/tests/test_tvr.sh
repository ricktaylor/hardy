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

# Configuration (ports are allocated dynamically below)
NODE1_NUM=1
NODE2_NUM=2
NODE3_NUM=3  # phantom node: no CLA, route-only
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

# Probe a TCP port with bash's /dev/tcp; success means something is listening.
port_open() {
    (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null
}

# Pick a TCP port nothing is listening on, avoiding ports already handed
# out. Kept below the Linux ephemeral floor (32768) so a transient
# outbound connection cannot grab the port between this probe and the
# server's bind; RANDOM caps at 32767 anyway, so 20000..32767 stays clear.
find_free_port() {
    local port used
    while :; do
        port=$(( (RANDOM % 12768) + 20000 ))
        for used in "$@"; do
            if [ "$port" -eq "$used" ]; then continue 2; fi
        done
        if ! port_open 127.0.0.1 "$port" && ! port_open ::1 "$port"; then
            echo "$port"
            return 0
        fi
    done
}

# Poll until a TCP port accepts connections, with a deadline in seconds.
# If a PID is given, fail fast when that process dies first.
wait_for_port() {
    local host=$1 port=$2 deadline=$3 label=$4 pid=${5:-}
    local waited=0
    while ! port_open "$host" "$port"; do
        if [ -n "$pid" ] && ! kill -0 "$pid" 2>/dev/null; then
            log_error "$label exited before listening on $host:$port"
            return 1
        fi
        if [ "$waited" -ge $((deadline * 10)) ]; then
            log_error "Timed out waiting for $label on $host:$port"
            return 1
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
    return 0
}

# Poll until a file contains at least N occurrences of a pattern, with a
# deadline in seconds.
wait_for_log() {
    local file=$1 pattern=$2 count=$3 deadline=$4
    local waited=0 seen
    while :; do
        seen=$(grep -c "$pattern" "$file" 2>/dev/null) || true
        if [ "${seen:-0}" -ge "$count" ]; then
            return 0
        fi
        if [ "$waited" -ge $((deadline * 10)) ]; then
            log_error "Timed out waiting for ${count}x '$pattern' in $file"
            return 1
        fi
        sleep 0.1
        waited=$((waited + 1))
    done
}

NODE1_TCPCLV4_PORT=$(find_free_port)
NODE2_TCPCLV4_PORT=$(find_free_port "$NODE1_TCPCLV4_PORT")
BPA_GRPC_PORT=$(find_free_port "$NODE1_TCPCLV4_PORT" "$NODE2_TCPCLV4_PORT")
TVR_GRPC_PORT=$(find_free_port "$NODE1_TCPCLV4_PORT" "$NODE2_TCPCLV4_PORT" "$BPA_GRPC_PORT")

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

# Build if needed. Set BUILD_PROFILE=debug for a faster local build.
# Only 'release' or 'debug' are valid: any other value would build debug
# but look under target/$BUILD_PROFILE/ and misreport a missing binary.
BUILD_PROFILE="${BUILD_PROFILE:-release}"
if [ "$BUILD_PROFILE" != "release" ] && [ "$BUILD_PROFILE" != "debug" ]; then
    log_error "BUILD_PROFILE must be 'release' or 'debug', got '$BUILD_PROFILE'"
    exit 1
fi
if [ "$SKIP_BUILD" = false ]; then
    log_step "Building Hardy binaries ($BUILD_PROFILE)..."
    cd "$WORKSPACE_DIR"
    if [ "$BUILD_PROFILE" = "release" ]; then
        cargo build --release -p hardy-tools -p hardy-bpa-server -p hardy-tvr
    else
        cargo build -p hardy-tools -p hardy-bpa-server -p hardy-tvr
    fi
fi

BP_BIN="$WORKSPACE_DIR/target/$BUILD_PROFILE/bp"
BPA_BIN="$WORKSPACE_DIR/target/$BUILD_PROFILE/hardy-bpa-server"
TVR_BIN="$WORKSPACE_DIR/target/$BUILD_PROFILE/hardy-tvr"

for bin in "$BP_BIN" "$BPA_BIN" "$TVR_BIN"; do
    if [ ! -x "$bin" ]; then
        log_error "Binary not found: $bin"
        exit 1
    fi
done

# grpcurl configuration for TVR gRPC session tests
GRPCURL_ARGS="-plaintext -import-path $WORKSPACE_DIR/tvr -import-path $WORKSPACE_DIR/proto -proto tvr.proto"
TVR_ADDR="[::1]:$TVR_GRPC_PORT"

SKIP_GRPC=false
if ! command -v grpcurl > /dev/null 2>&1; then
    log_warn "grpcurl not found; gRPC session tests (5-10) will be skipped"
    SKIP_GRPC=true
fi

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

# Node 1: has gRPC enabled (for hardy-tvr), TCPCLv4 for peering.
# debug logging so route installs/withdrawals are observable in the log.
cat > "$TEST_DIR/node1.toml" << EOF
log-level = "debug"
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
listeners = ["[::]:$NODE1_TCPCLV4_PORT"]
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
listeners = ["[::]:$NODE2_TCPCLV4_PORT"]
EOF

NODE1_LOG="$TEST_DIR/node1.log"
NODE2_LOG="$TEST_DIR/node2.log"
TVR_LOG="$TEST_DIR/tvr.log"

"$BPA_BIN" -c "$TEST_DIR/node1.toml" > "$NODE1_LOG" 2>&1 &
NODE1_PID=$!

"$BPA_BIN" -c "$TEST_DIR/node2.toml" > "$NODE2_LOG" 2>&1 &
NODE2_PID=$!

wait_for_port 127.0.0.1 "$NODE1_TCPCLV4_PORT" 20 "node 1 TCPCLv4" "$NODE1_PID" \
    || { cat "$NODE1_LOG"; exit 1; }
wait_for_port ::1 "$BPA_GRPC_PORT" 20 "node 1 gRPC" "$NODE1_PID" \
    || { cat "$NODE1_LOG"; exit 1; }
wait_for_port 127.0.0.1 "$NODE2_TCPCLV4_PORT" 20 "node 2 TCPCLv4" "$NODE2_PID" \
    || { cat "$NODE2_LOG"; exit 1; }
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

"$TVR_BIN" -c "$TEST_DIR/tvr.toml" > "$TVR_LOG" 2>&1 &
TVR_PID=$!

wait_for_port ::1 "$TVR_GRPC_PORT" 20 "hardy-tvr gRPC" "$TVR_PID" \
    || { cat "$TVR_LOG"; exit 1; }
log_info "hardy-tvr started with PID $TVR_PID"

# The route must actually reach Node 1's RIB, then the ping must succeed
if wait_for_log "$TVR_LOG" "Loaded contact plan" 1 15 \
    && wait_for_log "$NODE1_LOG" "Adding route ipn:$NODE2_NUM" 1 15 \
    && do_ping "ipn:$NODE2_NUM.7" "127.0.0.1:$NODE2_TCPCLV4_PORT" pass "Permanent route ping"
then
    TEST1=0
else
    TEST1=1
fi

# =============================================================================
# TEST 2: Hot-reload — add a route to phantom node
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 2: Hot-reload — add route to phantom node"
echo "============================================================"

# Add a route to a phantom node (no CLA peer, no echo service).
# We can't ping it, but we verify the reload happened and that the new
# route was actually installed in Node 1's RIB.
cat > "$TEST_DIR/contacts" << EOF
ipn:$NODE2_NUM.*.* via ipn:$NODE2_NUM.1.0 priority 10
ipn:$NODE3_NUM.*.* via ipn:$NODE3_NUM.1.0 priority 20
EOF

# Wait for debounce + reload, then check the route landed and the
# original route still works
if wait_for_log "$TVR_LOG" "Contact plan reloaded" 1 15 \
    && wait_for_log "$NODE1_LOG" "Adding route ipn:$NODE3_NUM" 1 15 \
    && do_ping "ipn:$NODE2_NUM.7" "127.0.0.1:$NODE2_TCPCLV4_PORT" pass "After hot-reload ping"
then
    TEST2=0
else
    TEST2=1
fi

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

TEST3=1
# Second reload: the Node 2 route must be withdrawn from Node 1's RIB
if wait_for_log "$TVR_LOG" "Contact plan reloaded" 2 15 \
    && wait_for_log "$NODE1_LOG" "Removed route ipn:$NODE2_NUM" 1 15
then
    # Delete the file: all TVR routes withdrawn
    rm -f "$TEST_DIR/contacts"

    # Ping the phantom node once withdrawal is done: should fail
    # (no route, no CLA peer)
    if wait_for_log "$TVR_LOG" "withdrawing all contacts" 1 15 \
        && wait_for_log "$NODE1_LOG" "Removed route ipn:$NODE3_NUM" 1 15 \
        && do_ping "ipn:$NODE3_NUM.7" "127.0.0.1:$NODE2_TCPCLV4_PORT" fail "Phantom node after file removal"
    then
        TEST3=0
    fi
fi

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

# Wait for the third reload and the Node 2 route to be re-installed
# (second "Adding route ipn:2" in Node 1's log), then ping again
if wait_for_log "$TVR_LOG" "Contact plan reloaded" 3 15 \
    && wait_for_log "$NODE1_LOG" "Adding route ipn:$NODE2_NUM" 2 15 \
    && do_ping "ipn:$NODE2_NUM.7" "127.0.0.1:$NODE2_TCPCLV4_PORT" pass "After file restore ping"
then
    TEST4=0
else
    TEST4=1
fi

# =============================================================================
# TEST 5: gRPC session open (TVR-01)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 5: gRPC session open"
echo "============================================================"

if [ "$SKIP_GRPC" = true ]; then
    log_warn "gRPC session open: SKIPPED (no grpcurl)"
    TEST5=skip
else

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

fi

# =============================================================================
# TEST 6: gRPC add contacts + route verification (TVR-05, TVR-09)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 6: gRPC add contacts via session"
echo "============================================================"

if [ "$SKIP_GRPC" = true ]; then
    log_warn "gRPC add contacts: SKIPPED (no grpcurl)"
    TEST6=skip
else

# First, remove the file-based contacts so only gRPC routes are active
rm -f "$TEST_DIR/contacts"
wait_for_log "$TVR_LOG" "withdrawing all contacts" 2 15 || true

# Open a session in background using a FIFO to keep the stream alive
ADD_FIFO="$TEST_DIR/add_fifo"
mkfifo "$ADD_FIFO"
tvr_grpcurl < "$ADD_FIFO" > "$TEST_DIR/grpc_output.json" 2>&1 &
GRPC_PID=$!
exec 4>"$ADD_FIFO"
echo '{"msg_id": 1, "open": {"name": "route-test", "default_priority": 100}}' >&4
echo '{"msg_id": 2, "add": {"contacts": [{"eid_pattern": "ipn:'"$NODE2_NUM"'.*.*", "via": "ipn:'"$NODE2_NUM"'.1.0", "priority": 10}]}}' >&4

# Verify the add response contains added count
if wait_for_log "$TEST_DIR/grpc_output.json" '"added":' 1 15; then
    log_info "gRPC add contacts: PASSED"
    TEST6=0
else
    log_error "gRPC add contacts: FAILED (no add response)"
    cat "$TEST_DIR/grpc_output.json" 2>/dev/null
    TEST6=1
fi

fi

# =============================================================================
# TEST 7: gRPC session close cleanup (TVR-09)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 7: gRPC session close — routes withdrawn"
echo "============================================================"

if [ "$SKIP_GRPC" = true ]; then
    log_warn "gRPC session close cleanup: SKIPPED (no grpcurl)"
    TEST7=skip
else

# Close fd 4 to close the FIFO, ending the grpcurl stream
exec 4>&-
if [ -n "$GRPC_PID" ] && kill -0 "$GRPC_PID" 2>/dev/null; then
    kill "$GRPC_PID" 2>/dev/null || true
    wait "$GRPC_PID" 2>/dev/null || true
fi
rm -f "$ADD_FIFO"

# Wait for TVR to process the stream close and withdraw routes
wait_for_log "$TVR_LOG" "Withdrawing contacts for session 'route-test'" 1 15 || true

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

fi

# =============================================================================
# TEST 8: gRPC duplicate session name (TVR-02)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 8: gRPC duplicate session name rejected"
echo "============================================================"

if [ "$SKIP_GRPC" = true ]; then
    log_warn "gRPC duplicate session name: SKIPPED (no grpcurl)"
    TEST8=skip
else

# Start a session in background using a FIFO to keep stdin open.
# Open fd 3 as a persistent writer so grpcurl's stdin stays open
# after we write the open message.
DUP_FIFO="$TEST_DIR/dup_fifo"
mkfifo "$DUP_FIFO"
tvr_grpcurl < "$DUP_FIFO" > /dev/null 2>&1 &
DUP_PID1=$!
exec 3>"$DUP_FIFO"
echo '{"msg_id": 1, "open": {"name": "dup-test", "default_priority": 100}}' >&3

# Wait for the first session to be registered before opening the duplicate
wait_for_log "$TVR_LOG" "TVR session opened: 'dup-test'" 1 15 || true

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

fi

# =============================================================================
# TEST 9: gRPC missing open (TVR-03)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 9: gRPC missing open — rejected"
echo "============================================================"

if [ "$SKIP_GRPC" = true ]; then
    log_warn "gRPC missing open: SKIPPED (no grpcurl)"
    TEST9=skip
else

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

fi

# =============================================================================
# TEST 10: gRPC session name reuse after close (TVR-12)
# =============================================================================
echo ""
echo "============================================================"
log_step "TEST 10: gRPC session name reuse after close"
echo "============================================================"

if [ "$SKIP_GRPC" = true ]; then
    log_warn "gRPC session name reuse: SKIPPED (no grpcurl)"
    TEST10=skip
else

# Open and close a session
echo '{"msg_id": 1, "open": {"name": "reuse-test", "default_priority": 100}}' \
    | tvr_grpcurl > /dev/null 2>&1 || true

# Wait for the close to be fully processed before re-opening
wait_for_log "$TVR_LOG" "Withdrawing contacts for session 'reuse-test'" 1 15 || true

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
SKIP=0

for t in TEST1 TEST2 TEST3 TEST4 TEST5 TEST6 TEST7 TEST8 TEST9 TEST10; do
    val=${!t:-1}
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
    case "$val" in
        0)
            echo "  $desc: PASS"
            PASS=$((PASS + 1))
            ;;
        skip)
            echo "  $desc: SKIP"
            SKIP=$((SKIP + 1))
            ;;
        *)
            echo "  $desc: FAIL"
            FAIL=$((FAIL + 1))
            ;;
    esac
done

echo ""
echo "  $PASS passed, $FAIL failed, $SKIP skipped"
echo ""

if [ "$FAIL" -eq 0 ]; then
    log_info "All TVR tests passed"
    exit 0
else
    log_error "Some tests failed"
    exit 1
fi
