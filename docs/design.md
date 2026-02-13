# Hardy Design

**Cloud-based DTN Router for Ground Systems**

## Introduction

Hardy is a modular, high-performance implementation of the Bundle Protocol version 7 (RFC 9171) written in Rust. The project follows a microservices architecture with loosely-coupled components, enabling independent deployment, replication, and autoscaling in cloud environments.

This division into loosely coupled components is current best practice when developing software for cloud environments, as it allows independent parts of the solution to be deployed, replicated, and autoscaled in line with active load. It also has the additional benefit that each component can be largely developed and tested independently in isolation.

## Design Goals

- **Modularity**: Components are separated into reusable library crates and application binaries
- **Performance**: Lock-free algorithms, async I/O, and zero-copy parsing where possible
- **Portability**: Core libraries marked `no_std` for embedded platform compatibility
- **Extensibility**: Trait-based APIs allow pluggable implementations for storage, CLAs, and services
- **Cloud-Native**: gRPC APIs, OpenTelemetry integration, and container-friendly configuration

## Architecture Overview

### Bundle Node Structure (RFC 9171 Section 3)

Hardy implements the bundle node architecture defined in RFC 9171 Section 3. A bundle node comprises an Application Agent, a Bundle Protocol Agent, and Convergence-Layer Adapters:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Bundle Node                                                                │
│                                                                             │
│  ┌───────────────────────────────────────────────────────────────────────┐  │
│  │  Application Agent                                                    │  │
│  │                                                                       │  │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                    │  │
│  │  │ Service 1   │  │ Service 2   │  │ Service n   │       . . .        │  │
│  │  │ (echo)      │  │ (user app)  │  │             │                    │  │
│  │  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘                    │  │
│  │         │                │                │                           │  │
│  └─────────┼────────────────┼────────────────┼───────────────────────────┘  │
│            │ ADUs           │ ADUs           │ ADUs                         │
│            │                │                │                              │
│      ┌─────┴────────────────┴────────────────┴─────┐                        │
│      │   Application/Service API (direct or gRPC)  │ ← hardy-proto          │
│      └─────┬────────────────┬────────────────┬─────┘                        │
│            │                │                │                              │
│  ┌─────────▼────────────────▼────────────────▼──────────────────────────┐   │
│  │                                                                      │   │
│  │     Bundle Protocol Agent (hardy-bpa)    ┌───────────────────────┐   |   |
│  │                                          │ Administrative        │   │   │
│  │                                          │ Element               │   │   │
│  │                                          └───────────────────────┘   │   │
│  │                                                                      │   │
│  └─────────┬────────────────┬────────────────┬──────────────────────────┘   │
│            │                │                │                              │
│      ┌─────┴────────────────┴────────────────┴─────┐                        │
│      │             CLA API (direct or gRPC)        │ ← hardy-proto          │
│      └─────┬────────────────┬────────────────┬─────┘                        │
│            │ Bundles        │ Bundles        │ Bundles                      │
│            │                │                │                              │
│  ┌─────────▼────────────────▼────────────────▼──────────────────────────┐   │
│  │  Convergence-Layer Adapters                                          │   │
│  │                                                                      │   │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                   │   │
│  │  │ CLA 1       │  │ CLA 2       │  │ CLA n       │       . . .       │   │
│  │  │ (TCPCLv4)   │  │ (File)      │  │ (BIBE)      │                   │   │
│  │  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘                   │   │
│  └─────────┼────────────────┼────────────────┼──────────────────────────┘   │
│            │                │                │                              │
└────────────┼────────────────┼────────────────┼──────────────────────────────┘
             │ CL PDUs        │ CL PDUs        │ CL PDUs
             ▼                ▼                ▼
       ┌──────────┐     ┌──────────┐     ┌──────────┐
       │ Network  │     │ Network  │     │ Network  │
       └──────────┘     └──────────┘     └──────────┘
```

Key aspects of Hardy's implementation:

- **Multiple services per BPA**: A single BPA instance supports many concurrent application services, each registered at its own endpoint
- **Administrative Element integrated into BPA**: Unlike RFC 9171's conceptual model where it's part of the Application Agent, Hardy compiles the Administrative Element directly into the BPA for efficiency
- **gRPC APIs**: Both service and CLA interfaces can operate over gRPC (hardy-proto) for distributed deployment, or in-process for standalone deployment

The BPA exchanges **ADUs** (Application Data Units) with services above and **Bundles** with CLAs below.

### BPA Internal Structure

The BPA is internally modular, with pluggable components for storage, routing, and filtering:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Bundle Protocol Agent (hardy-bpa)                                          │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐    │
│  │  Processing Pipeline                                                │    │
│  │                                                                     │    │
│  │  Ingress ──→ Validation ──→ Filtering ──→ Dispatch ──→ Egress       │    │
│  │                                                                     │    │
│  └─────────────────────────────────────────────────────────────────────┘    │
│                                                                             │
│  ┌───────────────────┐  ┌───────────────────┐  ┌───────────────────────┐    │
│  │  Routing (RIB)    │  │  Admin Endpoint   │  │  Filters              │    │
│  │                   │  │                   │  │                       │    │
│  │  Pattern-based    │  │  Status reports   │  │  Ingress / Egress     │    │
│  │  route lookup     │  │  generation       │  │  hooks                │    │
│  └───────────────────┘  └───────────────────┘  └───────────────────────┘    │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │  Storage (trait-based, pluggable)                                    │   │
│  │                                                                      │   │
│  │  ┌────────────────────────────┐  ┌────────────────────────────────┐  │   │
│  │  │  Bundle Data Storage       │  │  Metadata Storage              │  │   │
│  │  │                            │  │                                │  │   │
│  │  │  • localdisk-storage       │  │  • sqlite-storage              │  │   │
│  │  │  • (memory, for testing)   │  │  • (memory, for testing)       │  │   │
│  │  │  • (future: S3, etc.)      │  │  • (future: Postgres, etc.)    │  │   │
│  │  └────────────────────────────┘  └────────────────────────────────┘  │   │
│  └──────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│  ┌──────────────────────────────────────────────────────────────────────┐   │
│  │  Foundation Libraries                                                │   │
│  │                                                                      │   │
│  │  hardy-cbor    hardy-bpv7    hardy-eid-patterns    hardy-async       │   │
│  └──────────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────────┘
```

For detailed pipeline and subsystem documentation, see [bpa/docs/design.md](../bpa/docs/design.md).

## Package Summary

| Package | Type | Purpose | Design Doc |
|---------|------|---------|------------|
| hardy-cbor | Library | CBOR encoding/decoding | [cbor/docs/design.md](../cbor/docs/design.md) |
| hardy-bpv7 | Library | BPv7 bundle handling | [bpv7/docs/design.md](../bpv7/docs/design.md) |
| hardy-bpv7-tools | Application | Bundle CLI (bundle) | [bpv7/tools/docs/design.md](../bpv7/tools/docs/design.md) |
| hardy-eid-patterns | Library | EID pattern matching | [eid-patterns/docs/design.md](../eid-patterns/docs/design.md) |
| hardy-async | Library | Async runtime abstraction | [async/docs/design.md](../async/docs/design.md) |
| hardy-bpa | Library | Core BPA functionality | [bpa/docs/design.md](../bpa/docs/design.md) |
| hardy-proto | Library | gRPC definitions | [proto/docs/design.md](../proto/docs/design.md) |
| hardy-localdisk-storage | Library | Filesystem bundle storage | [localdisk-storage/docs/design.md](../localdisk-storage/docs/design.md) |
| hardy-sqlite-storage | Library | SQLite metadata storage | [sqlite-storage/docs/design.md](../sqlite-storage/docs/design.md) |
| hardy-tcpclv4 | Library | TCPCLv4 CLA | [tcpclv4/docs/design.md](../tcpclv4/docs/design.md) |
| hardy-file-cla | Library | File-based CLA | [file-cla/docs/design.md](../file-cla/docs/design.md) |
| hardy-echo-service | Library | Echo service | [echo-service/docs/design.md](../echo-service/docs/design.md) |
| hardy-otel | Library | OpenTelemetry integration | [otel/docs/design.md](../otel/docs/design.md) |
| hardy-ipn-legacy-filter | Library | Legacy IPN filter | [ipn-legacy-filter/docs/design.md](../ipn-legacy-filter/docs/design.md) |
| hardy-bibe | Library | BIBE implementation | [bibe/docs/design.md](../bibe/docs/design.md) |
| hardy-bpa-server | Application | BPA server | [bpa-server/docs/design.md](../bpa-server/docs/design.md) |
| hardy-tcpclv4-server | Application | Standalone TCPCLv4 | [tcpclv4-server/docs/design.md](../tcpclv4-server/docs/design.md) |
| hardy-tools | Application | CLI tools (bp) | [tools/docs/design.md](../tools/docs/design.md) |

## Testing

- [Test Strategy](test_strategy.md) - Master test plan with document hierarchy
- [Test Coverage Report](test_coverage_report.md) - Current test coverage status
- [Interoperability Test Plan](interop_test_plan.md) - Cross-implementation testing

### Unit Tests

Each library includes unit tests for core functionality, with examples drawn from relevant RFCs.

### Fuzz Testing

Fuzz targets exist for:

- CBOR parsing (`cbor/fuzz/`)
- EID parsing (`eid-patterns/fuzz/`)
- Bundle parsing (`bpv7/fuzz/`)
- BPA APIs (`bpa/fuzz/`)
- TCPCLv4 protocol parsing (`tcpclv4/fuzz/`)

### Integration Testing

- Cross-implementation testing with ION and other BPv7 implementations
- End-to-end testing via hardy-tools

## Dependencies and Compatibility

### Rust Edition

- Edition: 2024
- Minimum Rust version: 1.86

### Key External Dependencies

| Crate | Purpose |
|-------|---------|
| tokio | Async runtime |
| tonic | gRPC implementation |
| serde | Serialisation framework |
| tracing | Instrumentation |
| thiserror | Error handling |
| flume | Channel implementation |
| rusqlite | SQLite bindings |

### Platform Support

- **Full support**: Linux, macOS, Windows
- **Partial support** (`no_std` libraries): Embedded platforms with heap allocator

## Configuration

Configuration follows cloud-native patterns:

1. **Configuration files**: TOML, JSON, or YAML format
2. **Environment variables**: Override individual values
3. **Defaults**: Derived from relevant RFC specifications

Example configuration structure:

```toml
[node]
node_id = "ipn:1.0"

[storage]
bundle_dir = "/var/spool/hardy/bundles"
metadata_db = "/var/lib/hardy/metadata.db"

[tcpclv4]
listen_address = "0.0.0.0:4556"

[grpc]
enabled = true
listen_address = "127.0.0.1:50051"
```

## Deployment Models

### Standalone

Single process with all components linked:

```
hardy-bpa-server (with inline TCPCLv4)
```

### Distributed

Separate processes communicating via gRPC:

```
hardy-bpa-server <-> hardy-tcpclv4-server (multiple instances)
                 <-> Application services
```

This model allows:

- Multiple TCPCLv4 instances behind cloud load balancers, handling TCP/IP and CL processing before passing bundles to a single BPA
- Each application service in its own container for reliability, so a failure in one service does not compromise the system as a whole

### Embedded

Core libraries (`hardy-cbor`, `hardy-bpv7`) in `no_std` configuration for resource-constrained devices.
