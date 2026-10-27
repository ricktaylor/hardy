# hardy-tcpclv4-server Design

Standalone TCPCLv4 server application for distributed deployment.

**Status:** Skeleton implementation. The design below represents the target architecture; gRPC integration is not yet complete.

## Design Goals

- **Process isolation.** Run TCPCLv4 convergence layer processing in a separate process from the BPA. This provides fault isolation - a crash in the CLA doesn't bring down the BPA.

- **Scalable deployment.** Enable horizontal scaling by running multiple TCPCLv4 server instances behind load balancers. TCP connections are distributed across instances.

- **Configuration parity.** Support the same configuration patterns as hardy-bpa-server for operational consistency.

## Architecture Overview

The server combines hardy-tcpclv4 with hardy-proto's gRPC client:

```
                       ┌───────────────────────────────────────────┐
                       │         hardy-tcpclv4-server              │
  ┌─────────────┐      │  ┌─────────────────┐  ┌──────────────┐    │
  │ TCP Peers   │◄────►│  │  hardy-tcpclv4  │◄►│ gRPC Client  │────┼──► hardy-bpa-server
  │ (TCPCLv4)   │      │  │  (Cla trait)    │  │ (Sink proxy) │    │    (CLA service)
  └─────────────┘      │  └─────────────────┘  └──────────────┘    │
                       └───────────────────────────────────────────┘
```

Bundles received via TCPCLv4 are dispatched to the BPA through gRPC. Forwarding requests from the BPA arrive via gRPC and are transmitted over TCPCLv4 connections.

## Key Design Decisions

### gRPC as the Inter-Process Protocol

The server uses gRPC to communicate with the BPA rather than a custom protocol. This provides:

- Well-defined message framing and serialization (protobuf)
- Bidirectional streaming for the CLA protocol pattern
- Standard tooling for debugging and monitoring
- Potential for load balancing across multiple BPA instances

### Sink Proxy Pattern

The gRPC client implements the `Sink` trait, providing the same interface that in-process CLAs use. This means hardy-tcpclv4 doesn't need to know whether it's running in-process or as a separate server - the integration code is identical.

### Static Peers Are Kept Connected

Each entry in `peers` is resolved and dialled at startup, then dialled again whenever the entity holds no session to the dialled address: a peer that restarts, or a link that goes away and comes back, is reconnected without restarting this server. The server waits on `Tcpclv4::disconnected` rather than polling, and starts dials of one peer at least 5 seconds apart, so a peer that refuses or drops every session is retried at that pace. Dialling stops when the server is cancelled or the entity unregisters from the BPA.

Both ends may list each other. Each then forwards over the pool it dialled, which grows to `max-idle-connections` sessions, rather than over the session the other end dialled in (see the library's [Peer identity](../../tcpclv4/docs/design.md#peer-identity)), and keeping its own dial up after a peer restart keeps it that way.

This is the minimal keep-up target. The BPA's planned dial subsystem ([`dial_on_demand_plan.md`](../../bpa/docs/dial_on_demand_plan.md)) is meant to own keep-up targets, with backoff, for every CLA that can dial.

### Horizontal Scaling Model

Multiple server instances can run independently:

1. Each instance handles its own TCP connections
2. All instances connect to the same BPA via gRPC
3. Connection state is local to each instance
4. The BPA maintains a registry of which CLA instance has which peer connections

This separation allows network I/O to scale independently from bundle processing. High connection counts don't compete with CPU-bound cryptographic operations in the BPA.

## Configuration

Configuration uses the same patterns as hardy-bpa-server:

- **Formats** - TOML, JSON, or YAML
- **Environment overrides** - `HARDY_TCPCLV4_SERVER_*` prefix
- **Defaults** - Derived from RFC 9174 recommendations

Configuration owns the TCPCLv4 config-file schema (mapped onto `hardy-tcpclv4`'s `Tcpclv4::builder()`) plus the BPA gRPC endpoint address; the example configuration files (`example_config.toml`/`.yaml`) live in this crate.

## Integration

### With hardy-tcpclv4

The server builds a `hardy_tcpclv4::Tcpclv4` from the config-file mapping and passes it the gRPC-backed Sink. All TCPCLv4 protocol handling is delegated to the library.

### With hardy-proto

The hardy-proto package provides a `ClaSinkProxy` that implements the `Sink` trait over gRPC. This proxy handles the CLA gRPC client protocol, translating `Sink` trait calls (`dispatch()`, `add_peer()`, etc.) into gRPC stream messages to the BPA. The server instantiates this proxy and passes it to hardy-tcpclv4.

### With hardy-bpa-server

The BPA server hosts the CLA gRPC service. When this server registers via gRPC, the BPA treats it like any other CLA - sending forwarding requests and receiving dispatched bundles.

## Testing

- [Test Plan](test_plan.md) - Configuration, lifecycle, observability, cross-coverage from tcpclv4 library and interop tests
