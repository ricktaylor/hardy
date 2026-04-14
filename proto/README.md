# hardy-proto

gRPC proxy infrastructure for connecting remote CLAs, services, applications, and routing agents to a Hardy BPA.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

This crate provides Protobuf v3 definitions and tonic-based client/server implementations that allow BPA components to run in separate processes and communicate over gRPC. The server side plugs into a local `Bpa` instance; the client side exposes the same `BpaRegistration` trait so that components do not need to know whether the BPA is local or remote.

The crate uses an `RpcProxy` pattern with split reader/writer tasks and a bounded handler task pool for concurrent request processing.

## Features

- **Protobuf service definitions**: `cla.proto`, `service.proto`, `routing.proto` for all BPA registration interfaces
- **`RemoteBpa` client**: Implements `BpaRegistration` via gRPC, drop-in replacement for a local `Bpa`
- **gRPC server**: Exposes selectable registration endpoints (`cla`, `service`, `application`, `routing`)
- **RpcProxy**: Split reader/writer architecture with message-ID correlation and bounded handler pool
- **Stream-close unregistration**: No explicit unregister messages; stream closure drives cleanup
- Feature flag: `serde` -- enables serialization for server configuration
- Feature flag: `instrument` -- enables `tracing` span instrumentation

## Usage

```rust
// Client: connect to a remote BPA
use hardy_proto::client::RemoteBpa;

let remote_bpa = RemoteBpa::new("http://[::1]:50051".to_string());
cla.register(&remote_bpa, "tcp0".to_string(), None).await?;

// Server: expose a local BPA over gRPC
use hardy_proto::server;

server::init(&config, &bpa, &tasks);
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [API Documentation](https://docs.rs/hardy-proto)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
