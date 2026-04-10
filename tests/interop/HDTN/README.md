# NASA HDTN Interoperability Test

Bidirectional BPv7 bundle exchange between Hardy and NASA's
[HDTN](https://github.com/nasa/HDTN) implementation over TCPCLv4.

## Quick Start

```bash
# Full build + test
./tests/interop/HDTN/test_hdtn_ping.sh

# Skip Hardy rebuild (binaries already built)
./tests/interop/HDTN/test_hdtn_ping.sh --skip-build

# Custom ping count
./tests/interop/HDTN/test_hdtn_ping.sh --skip-build --count 10
```

## What the Test Does

**Test 1 — Hardy pings HDTN:** Hardy sends BPv7 echo requests to
`ipn:10.2047` via TCPCLv4.  HDTN's built-in echo service responds.
Hardy verifies round-trip delivery and reports RTT statistics.

**Test 2 — HDTN pings Hardy:** HDTN's `bping` tool sends BPv7 echo
requests to `ipn:1.7` via TCPCLv4.  Hardy's echo service responds.
`bping` runs standalone (no HDTN daemon required).

## Architecture

### Test 1 — Hardy pings HDTN

```mermaid
flowchart LR
    BP["bp ping"] -- "TCPCLv4" --> HDTN_BP["HDTN daemon"]
    HDTN_BP --> ECHO["Echo Service<br/>svc 2047"]
    ECHO --> HDTN_BP
    HDTN_BP -- "TCPCLv4" --> BP
```

### Test 2 — HDTN pings Hardy

```mermaid
flowchart LR
    BPING["bping"] -- "TCPCLv4" --> BPA["bpa-server"]
    BPA --> ECHO["Echo Service<br/>svc 7"]
    ECHO --> BPA
    BPA -- "TCPCLv4" --> BPING
```

## HDTN Modifications

None.  HDTN runs unmodified from upstream.

### Storage configuration

HDTN's storage is configured to write to `/dev/shm` (shared memory /
tmpfs) rather than disk.  HDTN does not offer an in-memory storage
option, but its config requires a `storageDiskConfigVector` file path.
Pointing this at `/dev/shm` avoids filesystem I/O during the echo test,
giving results that reflect protocol processing overhead rather than
disk latency.  The storage path is set in the `start_hdtn` entrypoint
script and has no effect on correctness — bundles flow via cut-through
for echo pings.

## Prerequisites

- Docker (builds the HDTN container image)
- Hardy `bp` and `hardy-bpa-server` binaries built

## Configuration

| Parameter | Value | Notes |
|-----------|-------|-------|
| HDTN node | `ipn:10.0` | Configurable via `NODE_ID` env var |
| Hardy node | `ipn:1.0` | |
| HDTN echo service | 2047 | HDTN's non-standard echo service number |
| Hardy echo service | 7 | Standard BPv7 echo service |
| TCPCLv4 port | 4556 | IANA standard; used by HDTN in Test 1, Hardy in Test 2 |
| TLS | Disabled | `must-use-tls = false` |
| Bundle signing | Disabled | `--no-sign` |
| Storage | `/dev/shm` | tmpfs — avoids disk I/O in benchmarks |

## File Layout

```
HDTN/
  test_hdtn_ping.sh        # Test runner
  start_hdtn.sh            # Interactive launcher (build + run)
  docker/
    Dockerfile             # Multi-stage HDTN build from upstream
    start_hdtn             # Container entrypoint (generates config + starts hdtn-one-process)
```
