# hardy-tools

Bundle Protocol diagnostic and testing tools.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Quick Start

```bash
# Build the tools
cargo build --release -p hardy-tools

# Ping a remote DTN node via TCPCLv4
bp ping ipn:2.0 192.168.1.10:4556

# Ping with a specific source EID
bp ping -S ipn:1.42 ipn:2.0 192.168.1.10:4556
```

## Subcommands

### `bp ping`

Send ping bundles to a destination endpoint and measure round-trip times.
Embeds a minimal BPA and establishes a CLA connection (TCPCLv4 by default).
Bundles are signed by default to detect corruption. Press Ctrl+C to stop
and show statistics.

```
bp ping [OPTIONS] <DESTINATION> [PEER]
```

**Examples:**

```bash
# Send 10 pings at 500ms intervals
bp ping -c 10 -i 500ms ipn:2.0 192.168.1.10:4556

# Ping with 1KB payload (MTU testing)
bp ping -s 1024 ipn:2.0 192.168.1.10:4556

# Ping with a 30-second timeout, quiet mode (summary only)
bp ping -q -w 30s ipn:2.0 192.168.1.10:4556

# Use an external CLA binary instead of built-in TCPCLv4
bp ping --cla /usr/bin/my-cla --cla-args "--port 5000" ipn:2.0

# Verbose output for debugging
bp ping -v=debug ipn:2.0 192.168.1.10:4556
```

**Options:**

| Option | Default | Description |
|--------|---------|-------------|
| `-c, --count` | unlimited | Number of pings to send |
| `-i, --interval` | `1s` | Interval between pings |
| `-s, --size` | -- | Target bundle size in bytes |
| `-w, --timeout` | -- | Total time limit for the session |
| `-W, --wait` | -- | Time to wait for responses after last ping |
| `-q, --quiet` | -- | Only show summary statistics |
| `-v, --verbose` | -- | Verbose output (`trace`, `debug`, `info`, `warn`, `error`) |
| `-t, --ttl` | -- | Hop limit (like IP TTL) |
| `--lifetime` | auto | Bundle lifetime |
| `-S, --source` | random | Source EID |
| `--cla` | `tcpclv4` | CLA name or path to external CLA binary |
| `--cla-args` | -- | Arguments for external CLA binary |
| `--grpc-listen` | `[::1]:50051` | gRPC listen address for external CLAs |
| `--no-sign` | -- | Disable BIB signing |
| `--no-payload-crc` | -- | Disable CRC on payload block (DTNME compat) |
| `--tls-insecure` | -- | Accept self-signed TLS certificates |
| `--tls-ca` | -- | CA bundle directory for TLS |

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/operations/tools/)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
