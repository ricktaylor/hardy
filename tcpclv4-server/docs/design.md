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

Configuration includes TCPCLv4-specific settings (from hardy-tcpclv4) plus the BPA gRPC endpoint address.

## Integration

### With hardy-tcpclv4

The server instantiates `hardy_tcpclv4::Cla` and passes it the gRPC-backed Sink. All TCPCLv4 protocol handling is delegated to the library.

### With hardy-proto

The hardy-proto package provides a `ClaSinkProxy` that implements the `Sink` trait over gRPC. This proxy handles the CLA gRPC client protocol, translating `Sink` trait calls (`dispatch()`, `add_peer()`, etc.) into gRPC stream messages to the BPA. The server instantiates this proxy and passes it to hardy-tcpclv4.

### With hardy-bpa-server

The BPA server hosts the CLA gRPC service. When this server registers via gRPC, the BPA treats it like any other CLA - sending forwarding requests and receiving dispatched bundles.
