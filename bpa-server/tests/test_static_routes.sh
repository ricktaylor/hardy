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

# Probe a TCP port with bash's /dev/tcp; success means something is listening.
port_open() {
    (exec 3<>"/dev/tcp/$1/$2") 2>/dev/null
}

# Pick a TCP port nothing is listening on.
find_free_port() {
    local port
    while :; do
        port=$(( (RANDOM % 20000) + 20000 ))
        if ! port_open 127.0.0.1 "$port" && ! port_open ::1 "$port"; then
            echo "$port"
            return 0
        fi
    done
}

# Poll until a TCP port accepts connections, with a deadline in seconds.
# Fails fast when the given process dies first.
wait_for_port() {
    local host=$1 port=$2 deadline=$3 label=$4 pid=$5
    local waited=0
    while ! port_open "$host" "$port"; do
        if ! kill -0 "$pid" 2>/dev/null; then
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

NODE_PORT=$(find_free_port)

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

# Set BUILD_PROFILE=debug for a faster local build.
BUILD_PROFILE="${BUILD_PROFILE:-release}"
if [ "$SKIP_BUILD" = false ]; then
    log_step "Building ($BUILD_PROFILE)..."
    cd "$WORKSPACE_DIR"
    if [ "$BUILD_PROFILE" = "release" ]; then
        cargo build --release -p hardy-tools -p hardy-bpa-server
    else
        cargo build -p hardy-tools -p hardy-bpa-server
    fi
fi

BP_BIN="$WORKSPACE_DIR/target/$BUILD_PROFILE/bp"
BPA_BIN="$WORKSPACE_DIR/target/$BUILD_PROFILE/hardy-bpa-server"

for bin in "$BP_BIN" "$BPA_BIN"; do
    [ -x "$bin" ] || { log_error "Not found: $bin"; exit 1; }
done

ROUTES_FILE="$TEST_DIR/static_routes"
FAILURES=0

# Initial routes file
cat > "$ROUTES_FILE" <<EOF
ipn:*.*.* drop
EOF

# Start BPA with echo + static routes + watch. debug logging so route
# installs/withdrawals ("Adding route"/"Removed route" from the RIB) are
# observable in the log.
cat > "$TEST_DIR/bpa.yaml" <<EOF
node-ids: "ipn:1.0"
log-level: debug
built-in-services:
  echo: [7]
static-routes:
  routes-file: "$ROUTES_FILE"
  watch: native
storage:
  metadata:
    type: memory
  bundle:
    type: memory
clas:
  - name: tcp0
    type: tcpclv4
    listeners: ["[::]:$NODE_PORT"]
EOF

BPA_LOG="$TEST_DIR/bpa.log"

log_step "Starting BPA server..."
"$BPA_BIN" --config "$TEST_DIR/bpa" > "$BPA_LOG" 2>&1 &
BPA_PID=$!
wait_for_port 127.0.0.1 "$NODE_PORT" 20 "BPA TCPCLv4" "$BPA_PID" \
    || { log_error "BPA failed to start"; cat "$BPA_LOG"; exit 1; }

# TEST 1: Startup, and the initial route actually lands in the RIB
log_step "TEST 1: Startup with routes file"
if wait_for_log "$BPA_LOG" "Adding route .*source 'static_routes'" 1 15; then
    log_info "TEST 1: PASSED"
else
    log_error "TEST 1: FAILED (initial static route not installed)"
    FAILURES=$((FAILURES + 1))
fi

# TEST 2: Hot-reload installs the new route
log_step "TEST 2: Hot-reload — modify routes file"
cat > "$ROUTES_FILE" <<EOF
ipn:*.*.* drop
ipn:99.*.* drop 3
EOF
if wait_for_log "$BPA_LOG" "Reloading static routes" 1 15 \
    && wait_for_log "$BPA_LOG" "Adding route ipn:99.*source 'static_routes'" 1 15 \
    && kill -0 "$BPA_PID" 2>/dev/null; then
    log_info "TEST 2: PASSED"
else
    log_error "TEST 2: FAILED (reload did not install the new route)"
    FAILURES=$((FAILURES + 1))
fi

# TEST 3: File removal withdraws both routes
log_step "TEST 3: File removal"
rm -f "$ROUTES_FILE"
if wait_for_log "$BPA_LOG" "Removed route .*source 'static_routes'" 2 15 \
    && kill -0 "$BPA_PID" 2>/dev/null; then
    log_info "TEST 3: PASSED"
else
    log_error "TEST 3: FAILED (routes not withdrawn after file removal)"
    FAILURES=$((FAILURES + 1))
fi

# TEST 4: File restore re-installs the route (second add of ipn:*.*.*)
log_step "TEST 4: File restore"
cat > "$ROUTES_FILE" <<EOF
ipn:*.*.* drop
EOF
if wait_for_log "$BPA_LOG" "Adding route .*source 'static_routes'" 3 15 \
    && kill -0 "$BPA_PID" 2>/dev/null; then
    log_info "TEST 4: PASSED"
else
    log_error "TEST 4: FAILED (route not re-installed after restore)"
    FAILURES=$((FAILURES + 1))
fi

# TEST 5: Ping echo — BPA still functional
log_step "TEST 5: Ping echo service"
if "$BP_BIN" ping "ipn:1.7" "127.0.0.1:$NODE_PORT" --count "$PING_COUNT" 2>&1; then
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
