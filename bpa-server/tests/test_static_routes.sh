#!/bin/bash
# Test: bpa-server static routes lifecycle
#
# Verifies SYS-05 (config reload) for the static routes subsystem.
# Single BPA with echo service — tests that the server starts, handles
# file changes gracefully, and remains functional throughout.
#
# Tests:
#   1. Startup with static routes file → BPA starts successfully
#   2. Hot-reload: modify routes file → BPA reloads without error
#   3. File removal: delete routes file → BPA handles gracefully
#   4. File restore: recreate routes file → BPA reloads without error
#   5. Ping echo service → BPA is functional after reload cycle
#
# Usage:
#   ./bpa-server/tests/test_static_routes.sh [--skip-build]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

NODE_PORT=4570
PING_COUNT=3

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_warn() { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }
log_step() { echo -e "${BLUE}[STEP]${NC} $*"; }

SKIP_BUILD=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --skip-build) SKIP_BUILD=true; shift ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

BPA_PID=""
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
            kill -9 "$pid" 2>/dev/null || true
        fi
        wait "$pid" 2>/dev/null || true
    fi
}

cleanup() {
    if [ -n "$CLEANUP_IN_PROGRESS" ]; then return; fi
    CLEANUP_IN_PROGRESS=1
    log_info "Cleaning up..."
    kill_process "$BPA_PID" "bpa-server"
    [ -n "$TEST_DIR" ] && [ -d "$TEST_DIR" ] && rm -rf "$TEST_DIR"
    log_info "Cleanup complete"
}
trap cleanup EXIT INT TERM

TEST_DIR=$(mktemp -d)
log_info "Test directory: $TEST_DIR"

if [ "$SKIP_BUILD" = false ]; then
    log_step "Building..."
    cd "$WORKSPACE_DIR"
    cargo build --release -p hardy-tools -p hardy-bpa-server
fi

BP_BIN="$WORKSPACE_DIR/target/release/bp"
BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"

for bin in "$BP_BIN" "$BPA_BIN"; do
    [ -x "$bin" ] || { log_error "Not found: $bin"; exit 1; }
done

ROUTES_FILE="$TEST_DIR/static_routes"
FAILURES=0

# Initial routes file
cat > "$ROUTES_FILE" <<EOF
ipn:*.*.* drop
EOF

# Start BPA with echo + static routes + watch
cat > "$TEST_DIR/bpa.yaml" <<EOF
node-ids: "ipn:1.0"
log-level: warn
built-in-services:
  echo: [7]
static-routes:
  routes-file: "$ROUTES_FILE"
  watch: true
storage:
  metadata:
    type: memory
  bundle:
    type: memory
clas:
  - name: tcp0
    type: tcpclv4
    address: "[::]:$NODE_PORT"
EOF

log_step "Starting BPA server..."
"$BPA_BIN" --config "$TEST_DIR/bpa" &
BPA_PID=$!
sleep 1
kill -0 "$BPA_PID" 2>/dev/null || { log_error "BPA failed to start"; exit 1; }

# TEST 1: Startup
log_step "TEST 1: Startup with routes file"
log_info "TEST 1: PASSED"

# TEST 2: Hot-reload
log_step "TEST 2: Hot-reload — modify routes file"
cat > "$ROUTES_FILE" <<EOF
ipn:*.*.* drop
ipn:99.*.* drop 3
EOF
sleep 2
kill -0 "$BPA_PID" 2>/dev/null && log_info "TEST 2: PASSED" || { log_error "TEST 2: FAILED"; FAILURES=$((FAILURES + 1)); }

# TEST 3: File removal
log_step "TEST 3: File removal"
rm -f "$ROUTES_FILE"
sleep 2
kill -0 "$BPA_PID" 2>/dev/null && log_info "TEST 3: PASSED" || { log_error "TEST 3: FAILED"; FAILURES=$((FAILURES + 1)); }

# TEST 4: File restore
log_step "TEST 4: File restore"
cat > "$ROUTES_FILE" <<EOF
ipn:*.*.* drop
EOF
sleep 2
kill -0 "$BPA_PID" 2>/dev/null && log_info "TEST 4: PASSED" || { log_error "TEST 4: FAILED"; FAILURES=$((FAILURES + 1)); }

# TEST 5: Ping echo — BPA still functional
log_step "TEST 5: Ping echo service"
if "$BP_BIN" ping "ipn:1.7" "127.0.0.1:$NODE_PORT" --count "$PING_COUNT" --no-sign 2>&1; then
    log_info "TEST 5: PASSED"
else
    log_error "TEST 5: FAILED"
    FAILURES=$((FAILURES + 1))
fi

echo ""
if [ $FAILURES -eq 0 ]; then
    log_info "All 5 tests passed"
else
    log_error "$FAILURES test(s) failed"
    exit 1
fi
