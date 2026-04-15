# Hardy DTN User Guide

Hardy is a performant, compliant, and extensible BPv7 Delay-Tolerant
Networking router for cloud-based ground systems.

!!! info "Project Status"
    Hardy targets full compliance with RFC 9171 and PICS conformance with CCSDS 734.20-O-1 (Bundle Protocol Version 7, Orange Book). The project is under active development.

## What is Hardy?

Hardy is a modular implementation of the Bundle Protocol Version 7
([RFC 9171](https://datatracker.ietf.org/doc/html/rfc9171)), written in
Rust for reliability and performance. It is designed for ground segment
operators and system integrators building DTN infrastructure for
satellite communications, deep-space links, and disruption-tolerant
networks.

## Key Features

- **Full RFC 9171 compliance** -- BPv7 bundle protocol with CCSDS PICS conformance
- **Bundle security** -- BPSec ([RFC 9172](https://datatracker.ietf.org/doc/html/rfc9172)/[9173](https://datatracker.ietf.org/doc/html/rfc9173)) with HMAC-SHA and AES-GCM
- **Multiple transport options** -- TCPCLv4 ([RFC 9174](https://datatracker.ietf.org/doc/html/rfc9174)), file-based, BIBE tunnelling
- **Pluggable storage** -- SQLite, PostgreSQL, local filesystem, Amazon S3
- **Time-variant routing** -- Contact scheduling with cron-based recurrence
- **Cloud-native** -- gRPC APIs, OpenTelemetry observability, OCI container images
- **Interoperable** -- Tested against 7+ BPv7 implementations (ION, HDTN, ud3tn, dtn7-rs, and others)
- **Extensible** -- Trait-based plugin architecture for CLAs, services, storage, and routing

## Getting Started

New to Hardy? Start here:

- [**Quick Start**](getting-started/quick-start.md) -- Get Hardy running with Docker in minutes
- [**Docker Deployment**](getting-started/docker.md) -- Production container setup

## Configuration

- [**BPA Server**](configuration/bpa-server.md) -- Node identity, gRPC, services, routing, and filters
- [**Storage Backends**](configuration/storage.md) -- Metadata and bundle data storage
- [**Convergence Layers**](configuration/convergence-layers.md) -- TCPCLv4 transport and TLS

## Operations

- [**CLI Tools**](operations/tools.md) -- `bp ping`, `bundle`, and `cbor` commands
- [**Observability**](operations/observability.md) -- Metrics, traces, and structured logging

## Recovery

- [**Recovery**](recovery/index.md) -- Crash recovery, storage backend behavior, and operator actions

## Other Documentation

- [**Design & Architecture**](https://github.com/ricktaylor/hardy/blob/main/docs/design.md) -- System architecture and design decisions (GitHub)
- [**Source Code**](https://github.com/ricktaylor/hardy) -- GitHub repository
