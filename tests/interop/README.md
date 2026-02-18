# Interoperability Tests

This directory contains tests for verifying Hardy's interoperability with other BPv7 implementations.

See also: [Interoperability Test Plan](../../docs/interop_test_plan.md)

## Directory Structure

```
tests/interop/
├── dtn7-rs/                   # dtn7-rs interoperability tests
│   ├── docker/                # Docker configuration
│   │   ├── Dockerfile.dtn7-rs # dtn7-rs build
│   │   └── start_dtnd         # Wrapper script for dtnd
│   ├── start_dtn7rs.sh        # Start dtn7-rs for interactive testing
│   └── test_dtn7rs_ping.sh    # dtn7-rs ping/echo test
└── README.md
```

## Quick Start

For interactive debugging, use `start_dtn7rs.sh`:

```bash
# Terminal 1: Start dtn7-rs
./tests/interop/dtn7-rs/start_dtn7rs.sh

# Terminal 2: Ping it
bp ping ipn:23.7 127.0.0.1:4556 --no-sign
```

## Tests

### dtn7-rs Ping/Echo (`test_dtn7rs_ping.sh`)

Tests bidirectional ping/echo between Hardy and [dtn7-rs](https://github.com/dtn7/dtn7-rs).

#### Prerequisites

- Docker (or local dtn7-rs installation with `--no-docker`)
- Rust toolchain (for building Hardy)

#### What It Tests

| Test | Description | Hardy Role | dtn7-rs Role |
|------|-------------|------------|--------------|
| **TEST 1** | Hardy pings dtn7-rs | Client (`bp ping`) | Server (dtnd + dtnecho2) |
| **TEST 2** | dtn7-rs pings Hardy | Server (bpa-server + echo) | Client (dtnsend) |

Both tests use TCPCLv4 as the convergence layer.

#### Usage

```bash
# Run full test (builds Hardy and Docker image)
./tests/interop/dtn7-rs/test_dtn7rs_ping.sh

# Skip cargo build (use existing binaries)
./tests/interop/dtn7-rs/test_dtn7rs_ping.sh --skip-build

# Use local dtnd/dtnecho2 instead of Docker
./tests/interop/dtn7-rs/test_dtn7rs_ping.sh --no-docker
```

#### Configuration

| Parameter | Value | Description |
|-----------|-------|-------------|
| Hardy Node | ipn:1.0 | Hardy's administrative endpoint |
| dtn7-rs Node | ipn:23.0 | dtn7-rs administrative endpoint |
| Hardy TCPCLv4 Port | 4557 | Port Hardy listens on (TEST 2) |
| dtn7-rs TCPCLv4 Port | 4556 | Port dtn7-rs listens on (TEST 1) |
| Echo Service | ipn:X.7 | Standard echo service number |

#### dtn7-rs Architecture

dtn7-rs uses a modular architecture where services connect to the daemon via WebSocket:

```
┌─────────────────────────────────────────┐
│              dtn7-rs Container          │
│                                         │
│  ┌─────────┐         ┌──────────┐       │
│  │  dtnd   │◄───────►│ dtnecho2 │       │
│  │         │   WS    │          │       │
│  │ TCPCLv4 │ :3000   │ ipn:23.7 │       │
│  │ :4556   │         └──────────┘       │
│  └────┬────┘                            │
└───────┼─────────────────────────────────┘
        │ TCPCLv4
        ▼
┌───────────────┐
│    Hardy      │
│  bp ping      │
│  ipn:1.0      │
└───────────────┘
```

**Key commands:**
- `dtnd -d -i0 -r epidemic -n 23 -C tcp:port=4556` - Start daemon with TCPCLv4
- `dtnecho2 -v` - Start echo service (connects to dtnd on localhost:3000)

#### Docker Image

The `Dockerfile.dtn7-rs` builds:
- `dtnd` - The dtn7-rs daemon
- `dtnecho2` - Echo service example
- `dtnsend` - Bundle sending utility
- `dtn7-plus` tools - Additional utilities

Build manually:
```bash
docker build -f tests/interop/dtn7-rs/docker/Dockerfile.dtn7-rs -t dtn7-interop tests/interop/dtn7-rs/docker
```

The image uses a `start_dtnd` wrapper that:
- Auto-detects broadcast addresses for discovery
- Uses `NODE_ID` env var for IPN naming scheme
- Passes additional arguments to dtnd

#### Expected Output

Successful TEST 1:
```
[INFO] TEST 1: dtn7-rs server with echo, Hardy pings
[STEP] Starting dtn7-rs daemon with TCPCLv4...
[INFO] Started dtn7-rs container: abc123def456
[STEP] Starting dtnecho2 service in container...
[STEP] Hardy pinging dtn7-rs echo service at ipn:23.7...

PING ipn:23.7 via 127.0.0.1:4556
64 bytes from ipn:23.7: seq=1 time=12.3ms
64 bytes from ipn:23.7: seq=2 time=8.1ms
...

[INFO] TEST 1 PASSED: Hardy successfully pinged dtn7-rs
```

#### Troubleshooting

**Container fails to start:**
```bash
# Check container logs
docker logs dtn7-interop-test

# Verify image was built
docker images | grep dtn7-interop
```

**Connection refused:**
- Verify ports 4556/4557 are not in use
- Check `--network host` is working (may need `--add-host` on some systems)

**Echo service not responding:**
- Ensure dtnecho2 had time to connect (script waits 2s)
- Check dtnd WebSocket is accessible on port 3000
