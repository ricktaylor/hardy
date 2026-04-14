# Hardy TCPCLv4 Server

A standalone server executable for the TCP Convergence Layer v4 (RFC 9174).

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

This component provides a TCP listener that spawns TCPCLv4 sessions for incoming connections.

## Usage

```bash
cargo run -p hardy-tcpclv4-server -- --help
```

```bash
hardy-tcpclv4-server --listen-address 0.0.0.0:4556
```

## Container Image

```bash
docker pull ghcr.io/ricktaylor/hardy/hardy-tcpclv4-server:latest
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/convergence-layers/)

## Licence

Apache 2.0 — see [LICENSE](../LICENSE)
