# hardy-tcpclv4

TCP Convergence Layer Protocol Version 4 library implementing [RFC 9174](https://datatracker.ietf.org/doc/html/rfc9174).

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Installation

```toml
[dependencies]
hardy-tcpclv4 = "0.4"
```

Published on [crates.io](https://crates.io/crates/hardy-tcpclv4).

## Overview

This crate implements TCPCLv4 as a CLA library for the Hardy BPA. It handles active and passive TCP session establishment, contact header exchange, SESS_INIT negotiation, bundle transfer segmentation, and keepalive management. TLS support is provided via `rustls` for encrypted connections.

The `Tcpclv4` type registers with any `BpaRegistration` implementation (local or remote via gRPC) and manages the full connection lifecycle including listener tasks, per-session state machines, and connection pooling. It is created through `Tcpclv4::builder()`, where every setting has a documented default; the configuration-file schema lives in the applications that consume this crate (`hardy-bpa-server`, `hardy-tcpclv4-server`).

## Features

- **Active and passive sessions**: Connect to peers or accept incoming connections with rate limiting
- **TLS support**: Optional TLS via `rustls` with configurable certificates, CA trust, and SNI
- **SESS_INIT negotiation**: Segment MRU, transfer MRU, and extension item exchange
- **Keepalive**: Configurable keepalive interval with RFC-compliant range warnings
- **Codec**: Encoder/decoder for all TCPCLv4 message types (XFER_SEGMENT, XFER_ACK, XFER_REFUSE, KEEPALIVE, SESS_TERM, MSG_REJECT, SESS_INIT)
- **Connection registry**: Idle connection pooling per remote address
- **Metrics**: 11 OpenTelemetry metrics for sessions, transfers, segments, throughput, and pool utilisation
- Feature flag: `instrument` -- enables `tracing` span instrumentation
- Feature flag: `serde` -- `Serialize`/`Deserialize` impls on the invariant newtypes (`ContactTimeout`, `KeepaliveInterval`) for consumer config schemas

## Usage

```rust
use hardy_tcpclv4::Tcpclv4;

// Every builder setting has a documented default; TLS material is
// built from tls::Tls::builder() and attached with .tls(..)
let cla = Arc::new(Tcpclv4::builder().build()?);

// Register with a BPA (local or remote), then dial a peer
bpa.register_cla("tcp0".to_string(), cla.clone(), None).await?;
cla.connect(&remote_addr).await?;

// Withdraw from the BPA without stopping the process: `unregister` asks
// the BPA to release the registration in-band. It is not needed for
// shutdown; tearing the registration down (the host's task teardown, or
// a remote session ending any way at all) unregisters just the same,
// and `on_unregister` fires exactly once either way.
cla.unregister().await;
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [Changelog](CHANGELOG.md)
- [API Documentation](https://docs.rs/hardy-tcpclv4)
- [User Documentation](https://ricktaylor.github.io/hardy/configuration/convergence-layers/)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
