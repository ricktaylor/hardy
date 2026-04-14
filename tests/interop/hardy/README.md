# Hardy-to-Hardy Interoperability Test

Baseline test: two Hardy BPA instances exchanging bundles over TCPCLv4.

This test establishes a performance and correctness baseline for the interop
suite. Both nodes are Hardy, so any failure here indicates a Hardy bug rather
than a cross-implementation compatibility issue.

## Quick Start

```bash
./tests/interop/hardy/test_hardy_ping.sh
./tests/interop/hardy/test_hardy_ping.sh --skip-build --count 10
```

## What the Test Does

**Test 1 — Node 1 pings Node 2:** Node 1's `bp ping` sends echo requests
to Node 2's echo service via TCPCLv4.

**Test 2 — Node 2 pings Node 1:** Node 2's `bp ping` sends echo requests
to Node 1's echo service via TCPCLv4.

No Docker containers are needed — both nodes run as local processes.

## Configuration

| Parameter | Value |
|-----------|-------|
| Node 1 | `ipn:1.0`, TCPCLv4 port 4560 |
| Node 2 | `ipn:2.0`, TCPCLv4 port 4561 |
| Echo service | 7 (standard) |

## File Layout

```
hardy/
  test_hardy_ping.sh         # Test runner (builds + runs both tests)
  run_hardy_ping_test.sh     # Single-direction ping helper
```
