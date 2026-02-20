# Hardy

A performant, compliant, and extensible BPv7 DTN solution for the Cloud.

[![Build](https://github.com/ricktaylor/hardy/actions/workflows/build.yml/badge.svg?branch=main)](https://github.com/ricktaylor/hardy/actions/workflows/build.yml)
[![Security audit](https://github.com/ricktaylor/hardy/actions/workflows/security_audit.yml/badge.svg)](https://github.com/ricktaylor/hardy/actions/workflows/security_audit.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2024%20edition-orange.svg)](https://www.rust-lang.org/)

## Overview

Hardy is a modular implementation of the Bundle Protocol Version 7 (BPv7) as defined in [RFC 9171](https://datatracker.ietf.org/doc/html/rfc9171), designed for Delay-Tolerant Networking (DTN) applications. Written in reliable, accessible, asynchronous Rust, many components are rigorously tested using fuzzing to ensure robustness and security.

## Table of Contents

- [Features](#features)
- [Components](#components)
  - [Core Libraries](#core-libraries)
  - [Storage Engines](#storage-engines)
  - [Convergence Layer Adapters](#convergence-layer-adapters)
  - [Services & Filters](#services--filters)
  - [Servers & Tools](#servers--tools)
- [Getting Started](#getting-started)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [License](#license)

## Features

- Full RFC 9171 BPv7 bundle protocol support
- BPSec (RFC 9172/9173) for bundle security with HMAC-SHA and AES-GCM
- Multiple convergence layer options (TCPCLv4, file-based, BIBE tunneling)
- Pluggable storage backends (SQLite, local filesystem)
- Configurable ingress/egress filter pipeline
- gRPC API for application integration
- OpenTelemetry integration for observability
- `no_std` compatible core libraries for embedded use

## Components

### Core Libraries

| Crate | Description |
|-------|-------------|
| [**`hardy-cbor`**](./cbor/) | RFC 8949 compliant Canonical CBOR encoder/decoder with streaming API. `no_std` compatible. |
| [**`hardy-bpv7`**](./bpv7/) | RFC 9171 BPv7 implementation with bundle creation, parsing, and manipulation. Includes BPSec support for integrity (BIB) and confidentiality (BCB) blocks. `no_std` compatible. |
| [**`hardy-bpa`**](./bpa/) | Complete Bundle Processing Agent library implementing DTN routing, dispatching, filter pipeline, RIB management, and CLA/service interfaces. |
| [**`hardy-eid-patterns`**](./eid-patterns/) | EID pattern parsing and matching for IPN and DTN URI schemes with glob support. |
| [**`hardy-async`**](./async/) | Runtime-agnostic async primitives including TaskPool, sync wrappers (Mutex, RwLock, Once), and cancellation tokens. |
| [**`hardy-proto`**](./proto/) | Protobuf v3 and gRPC API definitions for BPA-to-application and BPA-to-CLA communication. |
| [**`hardy-otel`**](./otel/) | OpenTelemetry integration for distributed tracing, metrics, and structured logging. |

### Storage Engines

| Crate | Description |
|-------|-------------|
| [**`hardy-sqlite-storage`**](./sqlite-storage/) | SQLite-based metadata storage engine with automatic schema migration. |
| [**`hardy-localdisk-storage`**](./localdisk-storage/) | Filesystem-based bundle storage with configurable fsync and recovery support. |

### Convergence Layer Adapters

| Crate | Description |
|-------|-------------|
| [**`hardy-tcpclv4`**](./tcpclv4/) | RFC 9174 TCPCLv4 implementation with TLS support, session management, and structured logging. |
| [**`hardy-file-cla`**](./file-cla/) | File-system-based CLA for bundle exchange via watched directories. |
| [**`hardy-bibe`**](./bibe/) | Bundle-in-Bundle Encapsulation (BIBE) for tunneling bundles through intermediate networks. |

### Services & Filters

| Crate | Description |
|-------|-------------|
| [**`hardy-echo-service`**](./echo-service/) | Echo service for testing and diagnostics - reflects bundles back to sender. |
| [**`hardy-ipn-legacy-filter`**](./ipn-legacy-filter/) | Egress filter for legacy 2-element IPN EID encoding compatibility. |

### Servers & Tools

| Crate | Description |
|-------|-------------|
| [**`hardy-bpa-server`**](./bpa-server/) | Modular BPv7 Bundle Processing Agent server with gRPC API, multiple storage backends, configurable filters, and static routing. |
| [**`hardy-tcpclv4-server`**](./tcpclv4-server/) | Standalone TCPCLv4 listener and session handler. |
| [**`hardy-bpv7-tools`**](./bpv7/tools/) | CLI (`bundle`) for bundle operations: create, inspect, validate, sign, encrypt, and more. |
| [**`hardy-cbor-tools`**](./cbor/tools/) | CLI (`cbor`) for CBOR inspection and conversion between binary, CDN, and JSON formats. |
| [**`hardy-tools`**](./tools/) | General DTN utilities including the `bp` command for ping and bundle operations. |

## Getting Started

### Prerequisites

- Rust 2024 edition (1.85+)
- Cargo

### Building

```bash
# Build all packages
cargo build --release

# Build the BPA server with all features
cargo build --release -p hardy-bpa-server --all-features

# Run tests
cargo test --workspace
```

### Running the BPA Server

```bash
# Run with a configuration file
./target/release/hardy-bpa-server -c config.yaml

# See available options
./target/release/hardy-bpa-server --help
```

See the [bpa-server README](./bpa-server/README.md) for detailed configuration options and example configurations.

### Bundle Tools

```bash
# Inspect a bundle
bundle inspect bundle.cbor

# Create a new bundle
bundle create --source dtn://node1/ --destination dtn://node2/ --payload "Hello DTN"

# Ping a remote node
bp ping ipn:2.0

# Inspect CBOR data
cbor inspect data.cbor
```

See the [bpv7-tools README](./bpv7/tools/README.md) and [cbor-tools README](./cbor/tools/README.md) for comprehensive usage guides.

## Key Documentation

| Document | Description |
|----------|-------------|
| [**Architecture Overview**](./docs/design.md) | High-level system architecture and design principles. |
| [**Requirements**](./docs/requirements.md) | High-level and low-level requirements with RFC traceability. |
| [**Test Strategy**](./docs/test_strategy.md) | Overall testing approach including unit, integration, and fuzz testing. |

## Contributing

We welcome contributions to the Hardy project! If you would like to contribute, please follow these guidelines:

1. Fork the repository and create a new branch for your contribution.
2. Make your changes and ensure that the code follows the project's coding style and conventions.
3. Write tests to cover your changes and ensure that all existing tests pass.
4. Submit a pull request with a clear description of your changes and the problem they solve.

Before contributing, please familiarize yourself with the project's [Test Strategy](./docs/test_strategy.md) to understand our approach to quality and verification.

By contributing to Hardy, you agree to license your contributions under the project's license.

## License

Hardy is licensed under the [Apache 2.0 License](./LICENSE).
