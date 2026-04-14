#!/bin/bash
# Test: hardy-otel OTLP export verification
#
# Verifies that hardy_otel::init() correctly exports traces, metrics,
# and logs to an OTLP collector.
#
# Uses a minimal OpenTelemetry Collector with file exporters and a
# small Rust test harness that emits telemetry.
#
# Requires: docker, jq, cargo
#
# Usage:
#   ./otel/tests/test_otel_export.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

OTEL_PORT=4317

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${GREEN}[INFO]${NC} $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }
log_step() { echo -e "${BLUE}[STEP]${NC} $*"; }

COLLECTOR_ID=""
CLEANUP_IN_PROGRESS=""

cleanup() {
    if [ -n "$CLEANUP_IN_PROGRESS" ]; then return; fi
    CLEANUP_IN_PROGRESS=1
    log_info "Cleaning up..."
    [ -n "$COLLECTOR_ID" ] && docker rm -f "$COLLECTOR_ID" 2>/dev/null || true
    [ -n "$TEST_DIR" ] && [ -d "$TEST_DIR" ] && rm -rf "$TEST_DIR"
    log_info "Cleanup complete"
}
trap cleanup EXIT INT TERM

for cmd in docker jq cargo; do
    command -v "$cmd" >/dev/null || { log_error "$cmd not found"; exit 1; }
done

TEST_DIR=$(mktemp -d)
OTEL_OUTPUT="$TEST_DIR/output"
mkdir -p "$OTEL_OUTPUT"
chmod 777 "$OTEL_OUTPUT"

log_info "Test directory: $TEST_DIR"

# --- Start OTEL Collector ---
log_step "Starting OTEL Collector..."

cat > "$TEST_DIR/otel-config.yaml" <<EOF
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: "0.0.0.0:4317"

exporters:
  file/traces:
    path: /output/traces.jsonl
  file/metrics:
    path: /output/metrics.jsonl
  file/logs:
    path: /output/logs.jsonl

service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [file/traces]
    metrics:
      receivers: [otlp]
      exporters: [file/metrics]
    logs:
      receivers: [otlp]
      exporters: [file/logs]
EOF

COLLECTOR_ID=$(docker run -d --rm \
    --name hardy-otel-export-test \
    -p "$OTEL_PORT:4317" \
    --mount type=bind,source="$TEST_DIR/otel-config.yaml",target=/etc/otelcol-contrib/config.yaml \
    --mount type=bind,source="$OTEL_OUTPUT",target=/output \
    otel/opentelemetry-collector-contrib:latest)

sleep 2

if ! docker ps -q --filter "id=$COLLECTOR_ID" | grep -q .; then
    log_error "OTEL Collector failed to start"
    docker logs hardy-otel-export-test 2>&1 | tail -10 || true
    exit 1
fi
log_info "Collector started"

# Verify collector is listening
log_step "Checking collector port..."
if ss -tlnp 2>/dev/null | grep -q ":$OTEL_PORT"; then
    log_info "Port $OTEL_PORT is listening"
else
    log_error "Port $OTEL_PORT is NOT listening"
    docker logs hardy-otel-export-test 2>&1 | tail -10 || true
    exit 1
fi

# --- Run test harness ---
log_step "Running OTEL export test harness..."

cd "$WORKSPACE_DIR"
export OTEL_EXPORTER_OTLP_ENDPOINT="http://127.0.0.1:$OTEL_PORT"
log_info "OTLP endpoint: $OTEL_EXPORTER_OTLP_ENDPOINT"
cargo test -p hardy-otel --test otel_export_test -- --ignored --nocapture 2>&1

# --- Check results ---
log_step "Output directory contents:"
ls -la "$OTEL_OUTPUT/" 2>&1 || true

FAILURES=0

# OTEL-01: Traces
log_step "OTEL-01: Checking traces..."
if [ -f "$OTEL_OUTPUT/traces.jsonl" ] && [ -s "$OTEL_OUTPUT/traces.jsonl" ]; then
    SPAN_COUNT=$(jq -s '[.[].resourceSpans[].scopeSpans[].spans | length] | add // 0' "$OTEL_OUTPUT/traces.jsonl" 2>/dev/null || echo 0)
    if [ "$SPAN_COUNT" -gt 0 ]; then
        log_info "OTEL-01: PASSED ($SPAN_COUNT spans exported)"
    else
        log_error "OTEL-01: FAILED (traces file exists but no spans found)"
        FAILURES=$((FAILURES + 1))
    fi
else
    log_error "OTEL-01: FAILED (no traces data)"
    FAILURES=$((FAILURES + 1))
fi

# OTEL-02: Metrics
log_step "OTEL-02: Checking metrics..."
if [ -f "$OTEL_OUTPUT/metrics.jsonl" ] && [ -s "$OTEL_OUTPUT/metrics.jsonl" ]; then
    METRIC_COUNT=$(jq -s '[.[].resourceMetrics[].scopeMetrics[].metrics | length] | add // 0' "$OTEL_OUTPUT/metrics.jsonl" 2>/dev/null || echo 0)
    if [ "$METRIC_COUNT" -gt 0 ]; then
        log_info "OTEL-02: PASSED ($METRIC_COUNT metrics exported)"
    else
        log_error "OTEL-02: FAILED (metrics file exists but no metrics found)"
        FAILURES=$((FAILURES + 1))
    fi
else
    log_error "OTEL-02: FAILED (no metrics data)"
    FAILURES=$((FAILURES + 1))
fi

# OTEL-03: Logs
log_step "OTEL-03: Checking logs..."
if [ -f "$OTEL_OUTPUT/logs.jsonl" ] && [ -s "$OTEL_OUTPUT/logs.jsonl" ]; then
    LOG_COUNT=$(jq -s '[.[].resourceLogs[].scopeLogs[].logRecords | length] | add // 0' "$OTEL_OUTPUT/logs.jsonl" 2>/dev/null || echo 0)
    if [ "$LOG_COUNT" -gt 0 ]; then
        log_info "OTEL-03: PASSED ($LOG_COUNT log records exported)"
    else
        log_error "OTEL-03: FAILED (logs file exists but no records found)"
        FAILURES=$((FAILURES + 1))
    fi
else
    log_error "OTEL-03: FAILED (no logs data)"
    FAILURES=$((FAILURES + 1))
fi

echo ""
if [ $FAILURES -eq 0 ]; then
    log_info "All 3 OTEL tests passed"
else
    log_error "$FAILURES OTEL test(s) failed"
    exit 1
fi
