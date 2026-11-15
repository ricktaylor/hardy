# hardy-bpa

Core Bundle Processing Agent library implementing [RFC 9171](https://datatracker.ietf.org/doc/html/rfc9171).

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Installation

```toml
[dependencies]
hardy-bpa = "0.2"
```

Published on [crates.io](https://crates.io/crates/hardy-bpa).

## Overview

This crate provides the bundle processing logic, storage abstractions, routing infrastructure, and component registries for a DTN node. It defines the traits that Convergence Layer Adapters (CLAs), services, applications, and routing agents implement to plug into the BPA, and coordinates bundle flow through ingress/egress filter pipelines.

The BPA is constructed via `BpaBuilder` and exposes a `BpaRegistration` trait that works identically whether the caller is in-process or connected over gRPC via `hardy-proto`.

## Features

- **Bundle routing**: RIB with ECMP, recursion detection, and reflection
- **Filter pipeline**: construction-frozen packs of Verifier (read-only), Classifier (metadata-annotating), and Rewriter (extension-block-editing) filters at four hook points: ingress, originate, egress, deliver
- **Pluggable storage**: `BundleStorage` and `MetadataStorage` traits with built-in in-memory implementations and optional LRU caching
- **Channel state machine**: Hybrid memory/storage backpressure for egress queues
- **Component registry**: Unified trait + sink pattern for CLAs, services, applications, and routing agents
- **Static routing**: `StaticRoutingAgent` for fixed route sets without implementing the full `RoutingAgent` trait
- **`no_std` compatible**: With a heap allocator (currently blocked on `flume` and `metrics` dependencies)
- Feature flag: `tokio` (default) -- enables Tokio runtime support, implies `std`
- Feature flag: `rfc9173` (default) -- enables RFC 9173 (BPSec) security contexts
- Feature flag: `serde` -- enables serialization support for metadata and configuration
- Feature flag: `instrument` -- enables `tracing` span instrumentation

## Usage

```rust
use hardy_bpa::bpa::Bpa;
use hardy_bpa::node_ids::NodeIds;

// Build a BPA with default in-memory storage, configuring the components
// it registers at start. A CLA crate's config converts into the CLA it
// describes; build() reports a bad config, or a clash, as an error.
let bpa = Bpa::builder()
    .node_ids(node_ids)
    .status_reports(true)
    .cla("tcp", tcpclv4_config, None)
    .application(service_id, app)
    .build()
    .await?;

// Start processing: the configured components go live
bpa.start(false).await;

// Components that arrive later register via the BpaRegistration trait
bpa.register_cla(name, cla, policy, init).await?;
bpa.register_routing_agent(name, agent).await?;

// One service on several endpoints, all or none: on_register is called
// once per endpoint, each with its own sink
bpa.register_service_endpoints(&[Service::Ipn(7), Service::Dtn("echo".into())], echo).await?;

// Clean shutdown
bpa.shutdown().await;
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [Changelog](CHANGELOG.md)
- [API Documentation](https://docs.rs/hardy-bpa)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
