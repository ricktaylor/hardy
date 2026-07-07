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
| [ud3tn](ud3tn/) | MTCP (via mtcp-cla) | AAP2 echo agent (service 7) | Passing | [README](ud3tn/README.md) |
| [ESA BP](ESA-BP/) | STCP (via mtcp-cla) | STCP CLE + echo (service 7) | Passing | [README](ESA-BP/README.md) |
| [NASA cFS](NASA-cFS/) | STCP (via mtcp-cla) | SB routing echo (service 7) | Passing | [README](NASA-cFS/README.md) |

## Running

### All tests

```bash
./tests/interop/run_all.sh
./tests/interop/run_all.sh --count 20        # custom ping count
./tests/interop/run_all.sh --skip-build      # skip Hardy cargo build
```

Results are saved to `interop_results.md`.

### Individual tests

```bash
./tests/interop/HDTN/test_hdtn_ping.sh
./tests/interop/HDTN/test_hdtn_ping.sh --skip-build --count 10
```

All test scripts support `--skip-build`, `--refresh` (rebuild the peer's Docker image from scratch), and `--count N`.

### Interactive testing

Each implementation has a `start_*.sh` script for manual testing:

```bash
# Terminal 1: Start the peer
./tests/interop/HDTN/start_hdtn.sh

# Terminal 2: Ping it
bp ping ipn:10.2047 127.0.0.1:4556
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

## Docker images

Every peer builds and runs in Docker. For reproducibility and a fair, apples-to-apples comparison, every peer except the two noted below **runs** on a single, digest-pinned base — `debian:trixie-slim` — so the OS and glibc are constant across implementations and only the implementation and its convergence layer vary. Build stages use that same base (layering GCC/CMake, OpenJDK, … on top), except dtn7-rs, whose build stage uses the maintained `rust:1.86` toolchain image.

**Run** stages are pinned by digest (`image:tag@sha256:…`) so the tested environment is reproducible. The peer Dockerfiles pin inline; Hardy's production Dockerfiles float on their tags — released images track base updates on rebuild — and the harness pins them by digest at test-build time (`tests/lib/docker_pins.sh`, which holds the same trixie digest the peers use). **Build** stages float on release-locked tags (`rust:1-slim-trixie`, `debian:trixie-slim`, …): Debian freezes library sonames within a stable release, so a same-release build and run stay ABI-consistent without pinning the throwaway toolchain — which also keeps it current and avoids stranding it on a version newer tooling has moved past. Implementations are separately pinned to known-good refs (each Dockerfile's `ARG *_REF`). (`ud3tn`'s build stays pinned, as its `python:3.13-slim` tag isn't release-locked.)

Exceptions:

- **DTNME** builds and runs on `debian:buster` — its binary links Boost 1.67 and OpenSSL 1.1, whose sonames exist only on that (now archived) release; moving forward would require porting DTNME itself.
- **ud3tn** builds and runs on `python:3.13-slim` (itself a trixie-based image) for its Python 3.13 echo agent.

**ESA-BP source:** ESA-BP is the one peer not built from a public git clone — it builds from a local checkout at `$ESA_BP_SRC` (default `../esa-bp`, an ESCL export). The pinned ref is checked out and proprietary-stripped before the build, and its reported version is captured from that ref. Without the checkout, `run_all.sh` reports ESA-BP as *not built* and continues; the standalone `test_esa_bp_ping.sh` errors clearly.

## Documentation

- [Test Plan](docs/test_plan.md)
- [Test Coverage](docs/test_coverage_report.md)
