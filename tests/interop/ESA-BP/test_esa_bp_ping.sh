#!/bin/bash
# Interoperability test: Hardy <-> ESA-BP ping/echo via STCP
#
# This script tests bidirectional ping/echo between Hardy and ESA-BP:
#   1. ESA-BP as server, Hardy pings it
#   2. Hardy as server with echo service, ESA-BP sends bundles to it
#
# The STCP CLE (4-byte length prefix framing) bridges ESA-BP and Hardy's
# MTCP CLA binary running in STCP mode.
#
# Prerequisites:
#   - Docker installed (for ESA-BP container)
#   - Hardy tools, bpa-server, and mtcp-cla built
#   - ESA-BP source at $ESA_BP_SRC (default: ../esa-bp)
#
# Usage:
#   ./tests/interop/ESA-BP/test_esa_bp_ping.sh [--skip-build] [--count N]

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

# shellcheck source=../lib/wait.sh
source "$SCRIPT_DIR/../lib/wait.sh"

ESA_BP_SRC="$(cd "${ESA_BP_SRC:-$WORKSPACE_DIR/../esa-bp}" 2>/dev/null && pwd || echo "${ESA_BP_SRC:-$WORKSPACE_DIR/../esa-bp}")"
# Current master commit (3.0.0.v20260521) for a fair, up-to-date interop comparison.
# The proprietary space-link CLs (SLE + generic-packetiser) are stripped after
# checkout by strip-proprietary.sh so the node builds from open Maven only.
ESA_BP_REF="${ESA_BP_REF:-f59410a90}"

# Configuration
HARDY_NODE_NUM=1
ESA_BP_NODE_NUM=10
ESA_BP_PORT=4558
HARDY_STCP_PORT=4557
HARDY_GRPC_PORT=50051
ESA_BP_BASE_IMAGE="esa-bp"
ESA_BP_IMAGE="esa-bp-interop"
PING_COUNT=5

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
ESA_BP_CONTAINER=""
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

    if [ -n "$ESA_BP_CONTAINER" ]; then
        docker stop -t 2 "$ESA_BP_CONTAINER" 2>/dev/null || true
        docker rm -f "$ESA_BP_CONTAINER" 2>/dev/null || true
    fi
    docker rm -f esa-bp-interop-test 2>/dev/null || true

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

# Build Hardy tools and server if needed
if [ "$SKIP_BUILD" = false ]; then
    log_step "Building Hardy tools and bpa-server..."
    cd "$WORKSPACE_DIR"
    cargo build --release -p hardy-tools -p hardy-bpa-server
    log_step "Building mtcp-cla..."
    cd "$WORKSPACE_DIR/tests/interop/mtcp"
    cargo build --release
fi

BP_BIN="$WORKSPACE_DIR/target/release/bp"
BPA_BIN="$WORKSPACE_DIR/target/release/hardy-bpa-server"
MTCP_BIN="$WORKSPACE_DIR/tests/interop/mtcp/target/release/mtcp-cla"

for bin in "$BP_BIN" "$BPA_BIN" "$MTCP_BIN"; do
    if [ ! -x "$bin" ]; then
        log_error "Binary not found: $bin"
        exit 1
    fi
done

# Build ESA-BP Docker images if needed
log_step "Checking for $ESA_BP_IMAGE Docker image..."
NOCACHE=""; if [ "$REFRESH" = true ]; then NOCACHE="--no-cache"; fi
if [ "$REFRESH" = true ] || ! docker image inspect "$ESA_BP_IMAGE" &>/dev/null; then
    if [ ! -d "$ESA_BP_SRC/src" ]; then
        log_error "ESA-BP source not found at $ESA_BP_SRC"
        log_error "Set ESA_BP_SRC to the ESA-BP source directory"
        exit 1
    fi

    # Pin the local checkout to a known-good release for reproducible interop.
    if git -C "$ESA_BP_SRC" rev-parse --git-dir >/dev/null 2>&1; then
        log_info "Checking out ESA-BP $ESA_BP_REF..."
        git -C "$ESA_BP_SRC" checkout --quiet "$ESA_BP_REF" 2>/dev/null \
            || log_warn "Could not checkout $ESA_BP_REF; using $(git -C "$ESA_BP_SRC" describe --tags --always 2>/dev/null)"
    fi

    # Capture the tested version from the pinned ref *before* strip-proprietary.sh
    # mutates the working tree, then bake it into the interop image (below) so
    # run_all.sh reads it uniformly from /interop-version like the other peers.
    # `git describe` without --dirty reports the checked-out commit; the strip
    # that follows is a deterministic, in-repo transform, not a source change to
    # flag.
    esa_desc="$(git -C "$ESA_BP_SRC" describe --tags --always 2>/dev/null || true)"
    esa_pom="$(grep -m1 -oE '<version>[^<]+</version>' "$ESA_BP_SRC/src/pom.xml" 2>/dev/null | sed -E 's#</?version>##g')"
    if [ -n "$esa_desc" ] && [ -n "$esa_pom" ]; then
        ESA_BP_VERSION="$esa_desc (declared: $esa_pom)"
    else
        ESA_BP_VERSION="${esa_desc:-${esa_pom:-source build}}"
    fi
    log_info "Tested ESA-BP version: $ESA_BP_VERSION"

    # Step 1: Build the base ESA-BP image.
    # 3.0.0's docker/Dockerfile unpacks a Maven-produced dist/bp-packager.zip (it
    # no longer compiles in-container), so we: reset to a clean checkout, strip the
    # proprietary space-link CLs, run the Maven build (Java 21) to produce the
    # packager zip, stage it in dist/, then build the image.
    if [ "$REFRESH" = true ] || ! docker image inspect "$ESA_BP_BASE_IMAGE" &>/dev/null; then
        log_info "Stripping proprietary CLs (SLE + generic-packetiser)..."
        git -C "$ESA_BP_SRC" checkout --quiet -- src 2>/dev/null || true   # clean base before strip
        bash "$SCRIPT_DIR/strip-proprietary.sh" "$ESA_BP_SRC"

        log_info "Building ESA-BP with Maven (Java 21; this may take a while)..."
        docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp -e MAVEN_CONFIG=/tmp/.m2 \
            -v "$ESA_BP_SRC":/ws -v "$HOME/.m2":/tmp/.m2 -w /ws/src \
            maven:3.9-eclipse-temurin-21 \
            mvn -Duser.home=/tmp -q -Dmaven.test.skip=true -T1C package

        log_info "Staging bp-packager.zip + building base $ESA_BP_BASE_IMAGE image..."
        mkdir -p "$ESA_BP_SRC/dist"
        cp "$ESA_BP_SRC"/src/target/bp-packager*.zip "$ESA_BP_SRC/dist/bp-packager.zip"
        docker build $NOCACHE -t "$ESA_BP_BASE_IMAGE" \
            -f "$ESA_BP_SRC/docker/Dockerfile" "$ESA_BP_SRC"
    else
        log_info "Using existing base $ESA_BP_BASE_IMAGE image"
    fi

    # Step 2: Layer our STCP CLE and start script on top
    log_info "Building $ESA_BP_IMAGE interop image..."
    docker build $NOCACHE -t "$ESA_BP_IMAGE" \
        --build-arg "BASE_IMAGE=$ESA_BP_BASE_IMAGE" \
        --build-arg "INTEROP_VERSION=$ESA_BP_VERSION" \
        -f "$SCRIPT_DIR/docker/Dockerfile" \
        "$SCRIPT_DIR"
else
    log_info "Using existing $ESA_BP_IMAGE image"
fi

# =============================================================================
# TEST 1: ESA-BP as server, Hardy pings it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 1: Hardy pings ESA-BP via STCP"
echo "============================================================"

# Start ESA-BP container with STCP, pointing at Hardy
docker rm -f esa-bp-interop-test 2>/dev/null || true

ESA_BP_CONTAINER=$(docker run -d \
    --name esa-bp-interop-test \
    --network host \
    -e NODE_ID="$ESA_BP_NODE_NUM" \
    -e STCP_LISTEN_PORT="$ESA_BP_PORT" \
    -e STCP_DEST_IP="127.0.0.1" \
    -e STCP_DEST_PORT="$HARDY_STCP_PORT" \
    -e REMOTE_NODE_ID="$HARDY_NODE_NUM" \
    "$ESA_BP_IMAGE")

log_info "Started ESA-BP container: ${ESA_BP_CONTAINER:0:12}"

# TEST 1 runs only if ESA-BP comes up. The `for … in 1` block lets any peer-side
# setup step `break` out recording FAIL, so a peer-side failure falls through to
# the summary instead of aborting the whole script.
TEST1_RESULT="FAIL"
for _ in 1; do
    # Wait until ESA-BP is accepting connections (see lib/wait.sh).
    log_info "Waiting for ESA-BP to accept connections on port $ESA_BP_PORT..."
    if ! wait_for_port 127.0.0.1 "$ESA_BP_PORT" 60 "$ESA_BP_CONTAINER"; then
        log_error "ESA-BP did not become ready on port $ESA_BP_PORT; skipping TEST 1"
        docker logs "$ESA_BP_CONTAINER" 2>&1 | tail -50
        break
    fi
    log_info "ESA-BP is accepting connections on port $ESA_BP_PORT"

    # Give ESA-BP time to finish internal setup after the port opens
    sleep 2

    # Start echo service inside ESA-BP container
    log_step "Starting echo service on ipn:$ESA_BP_NODE_NUM.7..."
    # Run the echo service against node.jar — the shaded uber jar that bundles the
    # gRPC runtime + stubs (the thin cli.jar does not include io.grpc).
    NODE_JAR=$(docker exec esa-bp-interop-test sh -c "find /opt/esa-bp -name 'node.jar' | head -1")
    docker exec -d esa-bp-interop-test sh -c \
        "java -Xmx128m -cp '$NODE_JAR:/opt/esa-bp/echo-service' EchoService 7 localhost 5672 > /tmp/echo.log 2>&1"
    sleep 3

    # Create CLA config for bp ping (inline BPA mode)
    cat > "$TEST_DIR/cla_ping.toml" << EOF
bpa-address = "http://[::1]:$HARDY_GRPC_PORT"
cla-name = "cl0"
framing = "stcp"
peer = "127.0.0.1:$ESA_BP_PORT"
peer-node = "ipn:$ESA_BP_NODE_NUM.0"
address = "0.0.0.0:$HARDY_STCP_PORT"
EOF

    # Hardy pings ESA-BP using bp ping with external CLA binary
    # Note: ESA-BP may not have an echo service, so we may get no responses
    log_step "Hardy pinging ESA-BP at ipn:$ESA_BP_NODE_NUM.7 via STCP..."
    echo ""

    PING_OUTPUT=$("$BP_BIN" ping "ipn:$ESA_BP_NODE_NUM.7" \
        --cla "$MTCP_BIN" \
        --cla-args "--config $TEST_DIR/cla_ping.toml" \
        --grpc-listen "[::1]:$HARDY_GRPC_PORT" \
        --source "ipn:$HARDY_NODE_NUM.12345" \
        --count "$PING_COUNT" \
        --timeout "$((PING_COUNT * 2 + 10))s" \
        2>&1) && EXIT_CODE=0 || EXIT_CODE=$?

    echo "$PING_OUTPUT"
    echo ""

    STATS_LINE=$(echo "$PING_OUTPUT" | grep -E '[0-9]+ (bundles )?transmitted' | head -1)
    TRANSMITTED=$(echo "$STATS_LINE" | sed -E 's/^([0-9]+).*/\1/')
    RECEIVED=$(echo "$STATS_LINE" | sed -E 's/.*, ([0-9]+) received.*/\1/')

    if [ $EXIT_CODE -eq 0 ] && [ "$RECEIVED" = "$TRANSMITTED" ] && [ -n "$RECEIVED" ]; then
        log_info "TEST 1 PASSED: Hardy successfully pinged ESA-BP ($RECEIVED/$TRANSMITTED)"
        TEST1_RESULT="PASS"
    elif [ $EXIT_CODE -eq 0 ]; then
        log_error "TEST 1 FAILED: Partial loss - only $RECEIVED/$TRANSMITTED responses received"
    elif [ $EXIT_CODE -eq 1 ]; then
        log_error "TEST 1 FAILED: No echo responses received (100% loss)"
    else
        log_error "TEST 1 FAILED: Error during ping (exit code $EXIT_CODE)"
    fi

    # Dump echo service log for debugging
    log_info "Echo service log:"
    docker exec esa-bp-interop-test cat /tmp/echo.log 2>/dev/null || true
    echo ""
done

# Stop ESA-BP for test 2
log_info "Stopping ESA-BP..."
docker stop "$ESA_BP_CONTAINER" 2>/dev/null || true
docker rm -f "$ESA_BP_CONTAINER" 2>/dev/null || true
ESA_BP_CONTAINER=""

sleep 1

# =============================================================================
# TEST 2: Hardy as server with echo, ESA-BP sends to it
# =============================================================================
echo ""
echo "============================================================"
log_info "TEST 2: ESA-BP sends bundles to Hardy echo service via STCP"
echo "============================================================"

# Create Hardy bpa-server config with gRPC enabled
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

log_step "Starting Hardy BPA server..."
"$BPA_BIN" -c "$TEST_DIR/hardy_config.toml" &
HARDY_PID=$!
sleep 2

if ! kill -0 "$HARDY_PID" 2>/dev/null; then
    log_error "Hardy BPA server failed to start"
    exit 1
fi
log_info "Hardy BPA server started with PID $HARDY_PID"

# Start MTCP CLA in STCP mode, connecting to Hardy's gRPC
cat > "$TEST_DIR/cla_server.toml" << EOF
bpa-address = "http://[::1]:$HARDY_GRPC_PORT"
cla-name = "cl0"
framing = "stcp"
address = "[::]:$HARDY_STCP_PORT"
peer = "127.0.0.1:$ESA_BP_PORT"
peer-node = "ipn:$ESA_BP_NODE_NUM.0"
EOF

log_step "Starting MTCP CLA (STCP mode) on port $HARDY_STCP_PORT..."
"$MTCP_BIN" -c "$TEST_DIR/cla_server.toml" &
MTCP_PID=$!
sleep 2

if ! kill -0 "$MTCP_PID" 2>/dev/null; then
    log_error "MTCP CLA failed to start"
    exit 1
fi
log_info "MTCP CLA started with PID $MTCP_PID"

# Start ESA-BP container pointing at Hardy
docker rm -f esa-bp-interop-test 2>/dev/null || true

ESA_BP_CONTAINER=$(docker run -d \
    --name esa-bp-interop-test \
    --network host \
    -e NODE_ID="$ESA_BP_NODE_NUM" \
    -e STCP_LISTEN_PORT="$ESA_BP_PORT" \
    -e STCP_DEST_IP="127.0.0.1" \
    -e STCP_DEST_PORT="$HARDY_STCP_PORT" \
    -e REMOTE_NODE_ID="$HARDY_NODE_NUM" \
    "$ESA_BP_IMAGE")

log_info "Started ESA-BP container: ${ESA_BP_CONTAINER:0:12}"

# TEST 2 records FAIL by default; a peer-side failure below falls through to the
# summary (only the Hardy-server/CLA-start failures above hard-exit, as harness
# errors).
TEST2_RESULT="FAIL"

# Wait until ESA-BP is accepting connections (see lib/wait.sh).
log_info "Waiting for ESA-BP to accept connections on port $ESA_BP_PORT..."
if wait_for_port 127.0.0.1 "$ESA_BP_PORT" 60 "$ESA_BP_CONTAINER"; then
    log_info "ESA-BP is accepting connections on port $ESA_BP_PORT"
else
    log_warn "ESA-BP not confirmed on port $ESA_BP_PORT; continuing"
fi

# Give ESA-BP time to finish internal setup after the port opens
sleep 2

if ! docker ps -q -f "id=$ESA_BP_CONTAINER" | grep -q .; then
    log_error "ESA-BP container exited unexpectedly. Logs:"
    docker logs "$ESA_BP_CONTAINER" 2>&1 | tail -50
    TEST2_RESULT="FAIL"
else
    # Use ESA-BP CLI bping to send bundles to Hardy's echo service
    log_step "ESA-BP bping to Hardy echo service at ipn:$HARDY_NODE_NUM.7..."

    # Generate a CLI config and logging properties for the container
    docker exec esa-bp-interop-test sh -c 'cat > /tmp/logging.properties << LOGEOF
handlers = java.util.logging.ConsoleHandler
.level = WARNING
java.util.logging.ConsoleHandler.level = WARNING
java.util.logging.ConsoleHandler.formatter = java.util.logging.SimpleFormatter
LOGEOF'

    docker exec esa-bp-interop-test sh -c "cat > /tmp/CLI.yml << 'CLIEOF'
grpc.address: localhost
grpc.port: 5672
grpc.client.secure.channel: false
grpc.client.logging.properties: /tmp/logging.properties
grpc.client.notifications.logging.path: /tmp/
client.service.number: 1
cli.bpcf.ADMINISTRATIVE: false
cli.bpcf.ACKNOWLEDGMENT: false
cli.bpcf.STATUS_TIME: false
cli.bpcf.RECEPTION_REPO: false
cli.bpcf.FORWARDING_REPO: false
cli.bpcf.DELIVERY_REPO: false
cli.bpcf.DELETION_REPO: false
cli.sr.repEid: ipn:${ESA_BP_NODE_NUM}.1
cli.sr.lifetime: 600000
CLIEOF"

    echo ""
    # ESA-BP CLI uses -flag=value syntax and is interactive.
    # bping checks stdin.available() to detect "press enter to stop".
    # Use -c=N to limit pings; sleep then send newline so the CLI exits cleanly.
    PING_OUTPUT=$(docker exec -i esa-bp-interop-test sh -c \
        "{ sleep $((PING_COUNT + 5)); echo ''; echo 'quit'; } | java -Xmx256m \
        -Djava.util.logging.config.file=/tmp/logging.properties \
        -Dcli.configuration=/tmp/CLI.yml \
        -Dclient.service.number=1 \
        -cp /opt/esa-bp/esa.egos.bp.cli/jars/cli.jar \
        esa.egos.bp.cli.CommandLineInterfaceGrpc \
        'bping -d=ipn:$HARDY_NODE_NUM.7 -c=$PING_COUNT -i=1'" \
        2>&1) || true

    echo "$PING_OUTPUT"
    echo ""

    if echo "$PING_OUTPUT" | grep -qi "error\|exception\|failed"; then
        log_error "TEST 2 FAILED: ESA-BP bping encountered errors"
        TEST2_RESULT="FAIL"
    else
        log_info "TEST 2 PASSED: ESA-BP sent bundles to Hardy"
        TEST2_RESULT="PASS"
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
echo "  TEST 1 (Hardy pings ESA-BP):       $TEST1_RESULT"
echo "  TEST 2 (ESA-BP sends to Hardy):    $TEST2_RESULT"
echo ""

if [ "$TEST1_RESULT" = "FAIL" ] || [ "$TEST2_RESULT" = "FAIL" ]; then
    log_error "Some tests failed"
    exit 1
else
    log_info "Interoperability tests completed"
    exit 0
fi
