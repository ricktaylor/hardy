# Interoperability Tests

Bidirectional BPv7 bundle exchange tests between Hardy and other implementations.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

Hardy is tested against 7 peer implementations. Each test verifies bidirectional bundle exchange: Hardy pings the peer's echo service, then the peer pings Hardy's echo service. See each implementation's README for architecture, configuration, and prerequisites.

| Implementation | CLA | Echo Service | Status | Details |
|---|---|---|---|---|
| [Hardy](hardy/) | TCPCLv4 | Built-in (service 7) | Passing | [README](hardy/README.md) (baseline) |
| [dtn7-rs](dtn7-rs/) | TCPCLv4 | dtnecho2 (service 7) | Passing | [README](dtn7-rs/README.md) |
| [HDTN](HDTN/) | TCPCLv4 | Built-in (service 2047) | Passing | [README](HDTN/README.md) |
| [DTNME](DTNME/) | TCPCLv4 | echo_me (service 7) | Passing | [README](DTNME/README.md) |
| [ION](ION/) | STCP (via mtcp-cla) | bpecho (service 7) | Passing | [README](ION/README.md) |
| [D3TN/ud3tn](ud3tn/) | MTCP (via mtcp-cla) | AAP2 echo agent (service 7) | Passing | [README](ud3tn/README.md) |
| [ESA BP](ESA-BP/) | STCP (via mtcp-cla) | STCP CLE + echo (service 7) | Passing | [README](ESA-BP/README.md) |
| [NASA cFS](NASA-cFS/) | STCP (custom PSP module) | SB routing echo (service 7) | Passing | [README](NASA-cFS/README.md) |

## Running

### All tests

```bash
./tests/interop/run_all.sh
./tests/interop/run_all.sh --count 20        # custom ping count
./tests/interop/run_all.sh --skip-build      # skip Hardy cargo build
```

Results are saved to `run_all_results.md`.

### Individual tests

```bash
./tests/interop/HDTN/test_hdtn_ping.sh
./tests/interop/HDTN/test_hdtn_ping.sh --skip-build --count 10
```

All test scripts support `--skip-build`, `--count N`, and `--no-docker`.

### Interactive testing

Each implementation has a `start_*.sh` script for manual testing:

```bash
# Terminal 1: Start the peer
./tests/interop/HDTN/start_hdtn.sh

# Terminal 2: Ping it
bp ping ipn:10.2047 127.0.0.1:4556 --no-sign
```

## MTCP/STCP CLA

The `mtcp/` directory contains a standalone CLA binary for implementations that use STCP or MTCP framing rather than TCPCLv4. It supports two modes:

- **STCP**: 4-byte big-endian u32 length prefix (ION, ESA BP, NASA cFS)
- **MTCP**: CBOR byte string framing (ud3tn/D3TN)

The CLA registers with Hardy's BPA via gRPC. It is workspace-excluded and must be built separately:

```bash
cd tests/interop/mtcp && cargo build --release
```

See [mtcp/design.md](mtcp/design.md) for wire format details.

## Documentation

- [Test Plan](docs/test_plan.md)
- [Test Coverage](docs/test_coverage_report.md)
