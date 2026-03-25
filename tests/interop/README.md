# Interoperability Tests

This directory contains tests for verifying Hardy's interoperability with other BPv7 implementations.

See also: [Interoperability Test Plan](../../docs/interop_test_plan.md)

## Overview

Hardy is tested against multiple other DTN Bundle Protocol implementations:

| Implementation | CLA | Echo Service | Status |
|---|---|---|---|
| [Hardy](hardy/) | TCPCLv4 | Built-in (service 7) | Passing |
| [dtn7-rs](dtn7-rs/) | TCPCLv4 | dtnecho2 (service 7) | Passing |
| [HDTN](HDTN/) | TCPCLv4 | Built-in (service 2047) | Passing |
| [DTNME](DTNME/) | TCPCLv4 | echo_me (service 7) | Passing |
| [ION](ION/) | STCP (via mtcp-cla) | bpecho (service 7) | Passing |
| [D3TN/ud3tn](ud3tn/) | MTCP (via mtcp-cla) | AAP2 echo agent (service 7) | Passing |
| [ESA BP](ESA-BP/) | STCP (via mtcp-cla) | STCP CLE + echo (service 7) | Passing |
| [NASA cFS BPNode](NASA-cFS/) | STCP (custom PSP module) | SB routing echo (service 7) | Passing |

Each test verifies bidirectional bundle exchange:

- **TEST 1**: Hardy pings the other implementation's echo service
- **TEST 2**: The other implementation pings Hardy's echo service

## Directory Structure

```
tests/interop/
├── README.md
├── benchmark.sh                  # Run all tests and compare RTT
├── mtcp/                         # MTCP/STCP CLA binary (for ION interop)
│   ├── Cargo.toml
│   ├── design.md                 # Wire format specification
│   └── src/                      # Rust source
├── hardy/                        # Hardy-to-Hardy tests
│   ├── test_hardy_ping.sh
│   └── run_hardy_ping_test.sh
├── dtn7-rs/                      # dtn7-rs interop tests
│   ├── docker/
│   │   ├── Dockerfile
│   │   └── start_dtnd
│   ├── start_dtn7rs.sh
│   └── test_dtn7rs_ping.sh
├── HDTN/                         # NASA HDTN interop tests
│   ├── docker/
│   │   ├── Dockerfile
│   │   └── start_hdtn
│   ├── start_hdtn.sh
│   └── test_hdtn_ping.sh
├── DTNME/                        # NASA DTNME interop tests
│   ├── docker/
│   │   ├── Dockerfile
│   │   ├── dtnme.cfg.template
│   │   └── start_dtnme
│   ├── start_dtnme.sh
│   └── test_dtnme_ping.sh
├── ION/                          # JPL ION interop tests
│   ├── docker/
│   │   ├── Dockerfile
│   │   └── start_ion
│   ├── start_ion.sh
│   └── test_ion_ping.sh
├── ud3tn/                        # D3TN/ud3tn interop tests
│   ├── docker/
│   │   └── start_ud3tn
│   ├── start_ud3tn.sh
│   └── test_ud3tn_ping.sh
├── ESA-BP/                       # ESA BP interop tests
│   ├── docker/
│   │   ├── Dockerfile
│   │   └── start_esa_bp
│   ├── start_esa_bp.sh
│   └── test_esa_bp_ping.sh
└── NASA-cFS/                     # NASA cFS BPNode interop tests
    ├── docker/
    │   ├── Dockerfile
    │   └── start_cfs
    ├── stcpsock_intf/            # STCP PSP module for cFS
    │   ├── stcpsock_intf.c
    │   └── CMakeLists.txt
    ├── cfs-config/               # cFS build/mission config overrides
    ├── start_cfs.sh
    └── test_cfs_ping.sh
```

## Quick Start

### Run All Tests (Benchmark)

```bash
# Run all interop tests and produce RTT comparison
./tests/interop/benchmark.sh

# Specify number of pings per test
./tests/interop/benchmark.sh --count 20

# Skip building Hardy binaries
./tests/interop/benchmark.sh --skip-build
```

### Run Individual Tests

```bash
# Full test (builds Hardy, builds Docker image if needed)
./tests/interop/HDTN/test_hdtn_ping.sh

# Skip cargo build
./tests/interop/HDTN/test_hdtn_ping.sh --skip-build

# Specify ping count
./tests/interop/HDTN/test_hdtn_ping.sh --count 10
```

### Interactive Testing

Each implementation has a `start_*.sh` script for manual testing:

```bash
# Terminal 1: Start the other implementation
./tests/interop/HDTN/start_hdtn.sh

# Terminal 2: Ping it (HDTN uses service 2047 for echo)
bp ping ipn:10.2047 127.0.0.1:4556 --no-sign
```

## MTCP/STCP CLA

The `mtcp/` directory contains a standalone CLA binary for ION interop testing. ION uses
STCP framing (4-byte big-endian u32 length prefix) rather than TCPCLv4, so Hardy needs
this separate CLA to communicate with it.

The binary supports two framing modes:

- **MTCP**: CBOR byte string framing (draft-ietf-dtn-mtcpcl-01), used by ud3tn/D3TN
- **STCP**: 4-byte big-endian u32 length prefix, used by ION

The CLA registers with Hardy's BPA via gRPC and can be used in two ways:

- **Standalone**: Run alongside `hardy-bpa-server` (registers via gRPC)
- **Inline**: Launched by `bp ping --cla /path/to/mtcp-cla --cla-args "--config config.toml"`

The mtcp-cla is excluded from the main cargo workspace and must be built separately:

```bash
cd tests/interop/mtcp && cargo build --release
```

See [mtcp/design.md](mtcp/design.md) for wire format details.

## Test Architecture

### TCPCLv4 Tests (dtn7-rs, HDTN, DTNME)

These tests use Hardy's built-in TCPCLv4 CLA:

```
TEST 1: Hardy pings other implementation
┌──────────┐  TCPCLv4   ┌──────────────┐
│ bp ping  │◄──────────►│  Other BPA   │
│ ipn:1.0  │   :4556    │  + echo svc  │
└──────────┘            └──────────────┘

TEST 2: Other implementation pings Hardy
┌──────────────┐  TCPCLv4   ┌──────────────┐
│ hardy-bpa    │◄──────────►│  Other BPA   │
│ + echo svc   │   :4557    │  + ping tool │
│ ipn:1.0      │            │              │
└──────────────┘            └──────────────┘
```

### STCP Tests (ION)

ION uses STCP framing, so Hardy needs the standalone mtcp-cla:

```
TEST 1: Hardy pings ION
┌──────────┐  gRPC   ┌───────────┐  STCP    ┌──────────┐
│ bp ping  │◄───────►│ mtcp-cla  │◄────────►│   ION    │
│ ipn:1.0  │ :50051  │ (STCP)    │  :4556   │ + bpecho │
└──────────┘         └───────────┘          └──────────┘

TEST 2: ION pings Hardy
┌──────────────┐  gRPC  ┌───────────┐  STCP   ┌──────────┐
│ hardy-bpa    │◄──────►│ mtcp-cla  │◄───────►│   ION    │
│ + echo svc   │:50051  │ (STCP)    │ :4557   │ + bpsrc  │
│ ipn:1.0      │        └───────────┘         └──────────┘
└──────────────┘
```

## Benchmark

The `benchmark.sh` script runs all available interop tests and produces an RTT comparison table:

Results are saved to `benchmark_results.md`.

## Common Options

All test scripts support:

| Option | Description |
|--------|-------------|
| `--skip-build` | Skip building Hardy binaries |
| `--count N` / `-c N` | Number of pings to send (default: 5) |
| `--no-docker` | Use local binaries instead of Docker (where supported) |

## Docker Images

All Docker images are built automatically on first test run. They clone source from
GitHub, so no local source is needed. Images are cached for subsequent runs.

To rebuild an image:

```bash
docker rmi hdtn-interop    # Remove cached image
./tests/interop/HDTN/test_hdtn_ping.sh   # Rebuilds automatically
```

Build args can be used to pin versions:

```bash
docker build -t hdtn-interop --build-arg HDTN_REF=v1.0.0 tests/interop/HDTN/docker
docker build -t dtn7-interop --build-arg DTN7_REF=v0.21.0 tests/interop/dtn7-rs/docker
docker build -t dtnme-interop --build-arg DTNME_REF=main tests/interop/DTNME/docker
```

## Troubleshooting

**Container fails to start:**

```bash
docker logs <container-name>    # Check container output
docker ps -a                    # Check container status
```

**Port conflicts:**

- Tests use `--network host` so ports must be free on the host
- Default ports: 4556 (other impl), 4557 (Hardy)
- Kill stale containers: `docker rm -f hdtn-interop-test`

**Build failures:**

- Ensure Docker has sufficient memory (HDTN needs ~4GB)
- Check network access to GitHub for cloning
