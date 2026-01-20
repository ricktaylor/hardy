# Hardy TCPCLv4 Server

A standalone server executable for the TCP Convergence Layer v4 (RFC 9174).

This component provides a TCP listener that spawns TCPCLv4 sessions for incoming connections.

## Usage

```bash
cargo run -p hardy-tcpclv4-server -- --help
```

```bash
hardy-tcpclv4-server --listen-address 0.0.0.0:4556
```
