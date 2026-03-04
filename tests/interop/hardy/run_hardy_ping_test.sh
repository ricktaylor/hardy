#!/bin/bash
# Run Hardy echo ping test against an already-running Hardy BPA (e.g. from compose).
# Does not spawn any servers or containers.
#
# Usage:
#   ./tests/interop/hardy/run_hardy_ping_test.sh [--peer HOST:PORT] [--count N]
#
# Options:
#   --peer HOST:PORT   Hardy TCPCL endpoint (default: hardy:4556)
#   --count N          Number of pings (default: 5)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

PEER="${PEER:-hardy:4556}"
PING_COUNT=5

while [[ $# -gt 0 ]]; do
    case $1 in
        --peer)
            PEER="$2"
            shift 2
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

HOST="${PEER%%:*}"
PORT="${PEER#*:}"

# Resolve bp binary
BP_BIN="${BP_BIN:-$WORKSPACE_DIR/target/release/bp}"
if [ ! -x "$BP_BIN" ]; then
    if command -v bp &>/dev/null; then
        BP_BIN=bp
    else
        echo "[ERROR] bp not found at $BP_BIN and not in PATH"
        exit 1
    fi
fi

echo "Waiting for Hardy at $HOST:$PORT..."
for i in $(seq 1 30); do
    if nc -z "$HOST" "$PORT" 2>/dev/null; then
        echo "Hardy is up"
        break
    fi
    echo "Hardy not ready yet, retry $i/30"
    sleep 2
done

if ! nc -z "$HOST" "$PORT" 2>/dev/null; then
    echo "[ERROR] Hardy did not become ready in time"
    exit 1
fi

# bp expects IP:port (no hostname); resolve if needed
if [[ "$HOST" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || [[ "$HOST" =~ ^\[.*\]$ ]]; then
    PEER_ADDR="$PEER"
else
    PEER_IP=$(getent hosts "$HOST" 2>/dev/null | awk '{print $1; exit}' || true)
    if [ -z "$PEER_IP" ]; then
        PEER_IP=$(awk -v h="$HOST" '$2 == h {print $1; exit}' /etc/hosts 2>/dev/null || true)
    fi
    if [ -z "$PEER_IP" ]; then
        echo "[ERROR] Could not resolve host: $HOST"
        exit 1
    fi
    PEER_ADDR="$PEER_IP:$PORT"
fi

echo "Running bp ping against Hardy echo at ipn:1.7 (peer $PEER_ADDR)..."
PING_OUTPUT=$("$BP_BIN" ping "ipn:1.7" "$PEER_ADDR" --count "$PING_COUNT" --no-sign 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
echo "$PING_OUTPUT"

STATS_LINE=$(echo "$PING_OUTPUT" | grep -E '[0-9]+ (bundles )?transmitted' | head -1)
TRANSMITTED=$(echo "$STATS_LINE" | sed -E 's/^([0-9]+).*/\1/')
RECEIVED=$(echo "$STATS_LINE" | sed -E 's/.*, ([0-9]+) received.*/\1/')

if [ "$EXIT_CODE" -eq 0 ] && [ -n "$RECEIVED" ] && [ "$RECEIVED" = "$TRANSMITTED" ]; then
    echo "[INFO] PASSED: $RECEIVED/$TRANSMITTED echo replies received"
    exit 0
else
    echo "[ERROR] FAILED: exit=$EXIT_CODE, received=$RECEIVED transmitted=$TRANSMITTED"
    exit 1
fi
