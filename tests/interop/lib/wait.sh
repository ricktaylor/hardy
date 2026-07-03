# Shared readiness helpers for the interop test suite.
#
# Sourced by run_all.sh and each peer's test_*_ping.sh to give every peer the
# same "has it started" check.

# wait_for_port HOST PORT [TIMEOUT_SECS] [CONTAINER]
#
# Block until a TCP connection to HOST:PORT succeeds — the definitive signal
# that the peer is accepting connections. Unlike `ss -tln | grep`, a real
# connect is independent of Docker's port-publishing mode (userland-proxy vs
# iptables) and never false-positives on a socket that is bound but not yet
# accepting. The probe opens and immediately closes the socket without writing,
# so it feeds no stray bytes to a length-prefixed (STCP/MTCP) or TCPCLv4 peer.
#
# If CONTAINER (a docker name or id) is given, the wait aborts as soon as that
# container stops running, so a crashed peer fails fast instead of waiting out
# the whole timeout.
#
# Returns 0 once connectable, 1 on timeout, 2 if the container exited.
wait_for_port() {
    local host="$1" port="$2" timeout_secs="${3:-30}" container="${4:-}"
    local i
    for (( i = 0; i < timeout_secs * 2; i++ )); do
        if [ -n "$container" ] \
            && [ "$(docker inspect -f '{{.State.Running}}' "$container" 2>/dev/null)" != "true" ]; then
            return 2
        fi
        if timeout 1 bash -c "exec 3<>/dev/tcp/$host/$port" 2>/dev/null; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}
