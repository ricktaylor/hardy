# Design: Hardy DTN Router

| Document Info | Details |
| ----- | ----- |
| **Project** | Hardy DTN Router |
| **Repository** | `github.com/ricktaylor/hardy` |
| **Version** | 1.1 |

## 1. Introduction

Hardy is a modular implementation of the Bundle Protocol version 7 (RFC 9171) written in Rust. The project follows a microservices architecture with loosely-coupled components that can be independently deployed, replicated, and autoscaled. This also allows each component to be developed and tested in isolation.

## 2. Design Goals

- **Modularity**: Components are separated into reusable library crates and application binaries
- **Performance**: Lock-free algorithms, async I/O, and zero-copy parsing where possible
- **Portability**: Core libraries marked `no_std` for embedded platform compatibility
- **Extensibility**: Trait-based APIs allow pluggable implementations for storage, CLAs, and services
- **Cloud-Native**: gRPC APIs, OpenTelemetry integration, and container-friendly configuration

## 3. Architecture Overview

### 3.1. Bundle Node Structure (RFC 9171 Section 3)

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

### 3.2. BPA Internal Structure

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

## 4. Package Summary

| Package | Type | Purpose | Design Doc |
|---------|------|---------|------------|
| hardy-cbor | Library | CBOR encoding/decoding | [cbor/docs/design.md](../cbor/docs/design.md) |
| hardy-bpv7 | Library | BPv7 bundle handling | [bpv7/docs/design.md](../bpv7/docs/design.md) |
| hardy-bpv7-tools | Application | Bundle CLI (`bundle`) | [bpv7/tools/docs/design.md](../bpv7/tools/docs/design.md) |
| hardy-cbor-tools | Application | CBOR CLI (`cbor`) | [cbor/tools/docs/design.md](../cbor/tools/docs/design.md) |
| hardy-eid-patterns | Library | EID pattern matching | [eid-patterns/docs/design.md](../eid-patterns/docs/design.md) |
| hardy-async | Library | Async runtime abstraction | [async/docs/design.md](../async/docs/design.md) |
| hardy-bpa | Library | Core BPA functionality | [bpa/docs/design.md](../bpa/docs/design.md) |
| hardy-proto | Library | gRPC definitions | [proto/docs/design.md](../proto/docs/design.md) |
| hardy-localdisk-storage | Library | Filesystem bundle storage | [localdisk-storage/docs/design.md](../localdisk-storage/docs/design.md) |
| hardy-sqlite-storage | Library | SQLite metadata storage | [sqlite-storage/docs/design.md](../sqlite-storage/docs/design.md) |
| hardy-postgres-storage | Library | PostgreSQL metadata storage | [postgres-storage/docs/design.md](../postgres-storage/docs/design.md) |
| hardy-s3-storage | Library | S3 bundle storage | [s3-storage/docs/design.md](../s3-storage/docs/design.md) |
| hardy-tcpclv4 | Library | TCPCLv4 CLA | [tcpclv4/docs/design.md](../tcpclv4/docs/design.md) |
| hardy-file-cla | Library | File-based CLA | [file-cla/docs/design.md](../file-cla/docs/design.md) |
| hardy-echo-service | Library | Echo service | [echo-service/docs/design.md](../echo-service/docs/design.md) |
| hardy-otel | Library | OpenTelemetry integration | [otel/docs/design.md](../otel/docs/design.md) |
| hardy-ipn-legacy-filter | Library | Legacy IPN filter | [ipn-legacy-filter/docs/design.md](../ipn-legacy-filter/docs/design.md) |
| hardy-bibe | Library | BIBE implementation | [bibe/docs/design.md](../bibe/docs/design.md) |
| hardy-tvr | Application | Time-Variant Routing agent | [tvr/docs/design.md](../tvr/docs/design.md) |
| hardy-bpa-server | Application | BPA server | [bpa-server/docs/design.md](../bpa-server/docs/design.md) |
| hardy-tcpclv4-server | Application | Standalone TCPCLv4 | [tcpclv4-server/docs/design.md](../tcpclv4-server/docs/design.md) |
| hardy-tools | Application | CLI tools (bp) | [tools/docs/design.md](../tools/docs/design.md) |

## 5. Testing

See the [Test Strategy](test_strategy.md) for the full verification approach, test plan inventory (32 plans), and tooling. The [Test Coverage Report](test_coverage_report.md) summarises current coverage across all crates.

## 6. Dependencies and Compatibility

### 6.1. Rust Edition

- Edition: 2024
- Minimum Rust version: 1.86

### 6.2. External Dependencies

| Crate | Purpose |
|-------|---------|
| tokio | Async runtime |
| tonic / prost | gRPC implementation and protobuf |
| serde | Serialisation framework |
| tracing | Instrumentation |
| opentelemetry | Metrics, traces, and logs export |
| flume | Channel implementation |
| rusqlite | SQLite bindings |
| tokio-postgres | PostgreSQL client |
| aws-sdk-s3 | S3-compatible object storage |
| rustls | TLS implementation |
| chumsky | Parser combinators (EID patterns, TVR) |
| humantime | Duration parsing |
| criterion | Performance benchmarking |

### 6.3. Platform Support

- **Full support**: Linux, macOS, Windows
- **Partial support** (`no_std` libraries): Embedded platforms with heap allocator

## 7. Configuration

Configuration uses the `config` crate with kebab-case field names:

1. **Configuration files**: YAML, TOML, or JSON format
2. **Environment variables**: Override individual values (prefix per binary, `__` for nesting)
3. **Defaults**: Sensible defaults derived from RFC specifications

See the [User Guide](https://ricktaylor.github.io/hardy/configuration/bpa-server/) for the full configuration reference.

## 8. Deployment Models

### 8.1. Standalone

Single process with all components linked:

```
hardy-bpa-server (with inline TCPCLv4)
```

### 8.2. Distributed

Separate processes communicating via gRPC:

```
hardy-bpa-server <-> hardy-tcpclv4-server (multiple instances)
                 <-> hardy-tvr (contact scheduling)
                 <-> Application services
```

This model allows:

- Multiple TCPCLv4 instances behind cloud load balancers, handling TCP/IP and CL processing before passing bundles to a single BPA
- Each application service in its own container for reliability, so a failure in one service does not compromise the system as a whole

### 8.3. Embedded

Core libraries (`hardy-cbor`, `hardy-bpv7`) in `no_std` configuration for resource-constrained devices.
