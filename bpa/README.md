# hardy-bpa

Core Bundle Processing Agent library implementing [RFC 9171](https://datatracker.ietf.org/doc/html/rfc9171).

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

This crate provides the bundle processing logic, storage abstractions, routing infrastructure, and component registries for a DTN node. It defines the traits that Convergence Layer Adapters (CLAs), services, applications, and routing agents implement to plug into the BPA, and coordinates bundle flow through ingress/egress filter pipelines.

The BPA is constructed via `BpaBuilder` and exposes a `BpaRegistration` trait that works identically whether the caller is in-process or connected over gRPC via `hardy-proto`.

## Features

- **Bundle routing**: RIB with ECMP, recursion detection, and reflection
- **Filter pipeline**: Read (parallel) and write (sequential) filters at four hook points: ingress, deliver, originate, egress
- **Pluggable storage**: `BundleStorage` and `MetadataStorage` traits with built-in in-memory implementations and optional LRU caching
- **Channel state machine**: Hybrid memory/storage backpressure for egress queues
- **Component registry**: Unified trait + sink pattern for CLAs, services, applications, and routing agents
- **Static routing**: `StaticRoutingAgent` for fixed route sets without implementing the full `RoutingAgent` trait
- **`no_std` compatible**: With a heap allocator (currently blocked on `flume` and `metrics` dependencies)
- Feature flag: `tokio` (default) -- enables Tokio runtime support, implies `std`
- Feature flag: `rfc9173` (default) -- enables RFC 9173 (BPSec) security contexts
- Feature flag: `serde` -- enables serialization support for metadata and configuration
- Feature flag: `instrument` -- enables `tracing` span instrumentation
- Feature flag: `no-rfc9171-autoregister` -- disables automatic registration of the RFC 9171 validity filter

## Usage

```rust
use hardy_bpa::bpa::Bpa;
use hardy_bpa::node_ids::NodeIds;

// Build a BPA with default in-memory storage
let bpa = Bpa::builder()
    .node_ids(node_ids)
    .status_reports(true)
    .build();

// Start processing
bpa.start(false);

// Register components via the BpaRegistration trait
bpa.register_cla(name, address_type, cla, policy).await?;
bpa.register_routing_agent(name, agent).await?;
bpa.register_application(service_id, app).await?;

// Clean shutdown
bpa.shutdown().await;
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [API Documentation](https://docs.rs/hardy-bpa)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
