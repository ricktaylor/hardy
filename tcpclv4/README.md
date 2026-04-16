# hardy-tcpclv4

TCP Convergence Layer Protocol Version 4 library implementing [RFC 9174](https://datatracker.ietf.org/doc/html/rfc9174).

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

This crate implements TCPCLv4 as a CLA library for the Hardy BPA. It handles active and passive TCP session establishment, contact header exchange, SESS_INIT negotiation, bundle transfer segmentation, and keepalive management. TLS support is provided via `rustls` for encrypted connections.

The `Cla` type registers with any `BpaRegistration` implementation (local or remote via gRPC) and manages the full connection lifecycle including listener tasks, per-session state machines, and connection pooling.

## Features

- **Active and passive sessions**: Connect to peers or accept incoming connections with rate limiting
- **TLS support**: Optional TLS via `rustls` with configurable certificates, CA trust, and SNI
- **SESS_INIT negotiation**: Segment MRU, transfer MRU, and extension item exchange
- **Keepalive**: Configurable keepalive interval with RFC-compliant range warnings
- **Codec**: Encoder/decoder for all TCPCLv4 message types (XFER_SEGMENT, XFER_ACK, XFER_REFUSE, KEEPALIVE, SESS_TERM, MSG_REJECT, SESS_INIT)
- **Connection registry**: Idle connection pooling per remote address
- **Metrics**: 11 OpenTelemetry metrics for sessions, transfers, segments, throughput, and pool utilisation
- Feature flag: `serde` -- enables serialization for configuration structs
- Feature flag: `instrument` -- enables `tracing` span instrumentation

## Usage

```rust
use hardy_tcpclv4::{Cla, config::Config};

let config = Config::default();
let cla = Arc::new(Cla::new(&config)?);

// Register with BPA (local or remote)
cla.register(&bpa, "tcp0".to_string(), None).await?;

// Connect to a remote peer
cla.connect(&remote_addr).await?;

// Clean shutdown
cla.unregister().await;
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [API Documentation](https://docs.rs/hardy-tcpclv4)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/convergence-layers/)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
