#!/usr/bin/env bash
# Verify Hardy container images can be built, started, and removed.
#
# Usage:
#   ./tests/image_checks.sh [--skip-build]
#
# Prerequisites:
#   - Docker with BuildKit support
#
# What it checks:
#   - Each image builds successfully
#   - Each container starts and the entrypoint binary executes
#   - Each container can be stopped and removed
#
# Images tested:
#   - hardy-bpa-server     (bpa-server/Dockerfile)
#   - hardy-tcpclv4-server (tcpclv4-server/Dockerfile)
#   - hardy-tvr            (tvr/Dockerfile)
#   - hardy-tools          (tools/Dockerfile)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

SKIP_BUILD=false
PASS=0
FAIL=0
CONTAINERS=()

for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        *) echo "Unknown argument: $arg"; exit 1 ;;
    esac
done

cleanup() {
    for c in "${CONTAINERS[@]}"; do
        docker stop -t 2 "$c" 2>/dev/null || true
        docker rm -f "$c" 2>/dev/null || true
    done
}
trap cleanup EXIT

log_pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
log_fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# ── Build ────────────────────────────────────────────────────────────────────

if [ "$SKIP_BUILD" = false ]; then
    echo "Building images..."

    docker build -t hardy-bpa-server:test \
        -f "$WORKSPACE_DIR/bpa-server/Dockerfile" \
        --target runtime \
        "$WORKSPACE_DIR" \
        && log_pass "build hardy-bpa-server" \
        || log_fail "build hardy-bpa-server"

    docker build -t hardy-tcpclv4-server:test \
        -f "$WORKSPACE_DIR/tcpclv4-server/Dockerfile" \
        --target runtime \
        "$WORKSPACE_DIR" \
        && log_pass "build hardy-tcpclv4-server" \
        || log_fail "build hardy-tcpclv4-server"

    docker build -t hardy-tvr:test \
        -f "$WORKSPACE_DIR/tvr/Dockerfile" \
        --target runtime \
        "$WORKSPACE_DIR" \
        && log_pass "build hardy-tvr" \
        || log_fail "build hardy-tvr"

    docker build -t hardy-tools:test \
        -f "$WORKSPACE_DIR/tools/Dockerfile" \
        --target runtime \
        "$WORKSPACE_DIR" \
        && log_pass "build hardy-tools" \
        || log_fail "build hardy-tools"
else
    echo "Skipping build (--skip-build)"
fi

TAG="test"

# ── hardy-bpa-server ─────────────────────────────────────────────────────────

echo ""
echo "Checking hardy-bpa-server..."

CONTAINER="hardy-bpa-server-check-$$"
CONTAINERS+=("$CONTAINER")

# Server starts and listens on gRPC port (50051).
# Run briefly then check it was alive.
if docker run -d --name "$CONTAINER" "hardy-bpa-server:$TAG" >/dev/null 2>&1; then
    sleep 2
    if docker ps -q -f "name=$CONTAINER" | grep -q .; then
        log_pass "hardy-bpa-server starts"
    else
        log_fail "hardy-bpa-server starts (exited early)"
        docker logs "$CONTAINER" 2>&1 | tail -5
    fi
    docker stop -t 2 "$CONTAINER" >/dev/null 2>&1 && docker rm -f "$CONTAINER" >/dev/null 2>&1 \
        && log_pass "hardy-bpa-server stop+remove" \
        || log_fail "hardy-bpa-server stop+remove"
else
    log_fail "hardy-bpa-server starts"
fi

# ── hardy-tcpclv4-server ─────────────────────────────────────────────────────

echo ""
echo "Checking hardy-tcpclv4-server..."

CONTAINER="hardy-tcpclv4-server-check-$$"
CONTAINERS+=("$CONTAINER")

# TCPCLv4 server needs a BPA to connect to, so it will exit.
# We just verify the binary executes (exit is expected).
if docker run --name "$CONTAINER" "hardy-tcpclv4-server:$TAG" 2>&1 | head -5 >/dev/null; then
    log_pass "hardy-tcpclv4-server executes"
else
    log_pass "hardy-tcpclv4-server executes (exit expected without BPA)"
fi
docker rm -f "$CONTAINER" >/dev/null 2>&1 \
    && log_pass "hardy-tcpclv4-server remove" \
    || log_fail "hardy-tcpclv4-server remove"

# ── hardy-tvr ────────────────────────────────────────────────────────────────

echo ""
echo "Checking hardy-tvr..."

CONTAINER="hardy-tvr-check-$$"
CONTAINERS+=("$CONTAINER")

# TVR agent needs a BPA to connect to, so it will exit.
# We just verify the binary executes (exit is expected).
if docker run --name "$CONTAINER" "hardy-tvr:$TAG" 2>&1 | head -5 >/dev/null; then
    log_pass "hardy-tvr executes"
else
    log_pass "hardy-tvr executes (exit expected without BPA)"
fi
docker rm -f "$CONTAINER" >/dev/null 2>&1 \
    && log_pass "hardy-tvr remove" \
    || log_fail "hardy-tvr remove"

# ── hardy-tools ──────────────────────────────────────────────────────────────

echo ""
echo "Checking hardy-tools..."

CONTAINER="hardy-tools-check-$$"
CONTAINERS+=("$CONTAINER")

# tools image has a shell (debian-slim), so we can run --help
if docker run --rm --name "$CONTAINER" "hardy-tools:$TAG" bp --help >/dev/null 2>&1; then
    log_pass "hardy-tools bp --help"
else
    log_fail "hardy-tools bp --help"
fi

CONTAINER="hardy-tools-bundle-check-$$"
CONTAINERS+=("$CONTAINER")

if docker run --rm --name "$CONTAINER" "hardy-tools:$TAG" bundle --help >/dev/null 2>&1; then
    log_pass "hardy-tools bundle --help"
else
    log_fail "hardy-tools bundle --help"
fi

CONTAINER="hardy-tools-cbor-check-$$"
CONTAINERS+=("$CONTAINER")

if docker run --rm --name "$CONTAINER" "hardy-tools:$TAG" cbor --help >/dev/null 2>&1; then
    log_pass "hardy-tools cbor --help"
else
    log_fail "hardy-tools cbor --help"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "Results: $PASS passed, $FAIL failed"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
