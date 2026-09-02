# hardy-proto

The gRPC wire contract of the Hardy BPA: versioned protobuf schemas and their generated types, for connecting remote applications, endpoint services, CLAs, and routing agents to a BPA.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Installation

```toml
[dependencies]
hardy-proto = "0.3"
```

Published on [crates.io](https://crates.io/crates/hardy-proto).

## Overview

One gRPC service per component surface, defined in the schemas under [`proto/`](./proto/) and exposed as generated types (one root module per protobuf package):

- `application.v1` — applications: ADUs in and out.
- `service.v1` — low-level endpoint services: whole BPv7 bundles in and out.
- `cla.v1` — convergence-layer adapters.
- `routing.v1` — routing agents.

Every surface follows the same design. `Subscribe` is the session: `Register` commences the registration, the BPA answers with `Registration` carrying a session token, and from then on the down direction is a pure event stream of small messages. Every action a component takes is an ordinary RPC presenting the token, and payload bytes move only on the chunked streaming data-plane calls (`Send`, `Receive`, `Dispatch`, `Forward`), with in-stream cancellation in both directions. Closing the session stream terminates the registration and invalidates the token.

The schemas speak RFC 9171 vocabulary throughout, and the RFC 9171 bundle id is the one identity across delivery announcements, collection, send results, and bundle status reports. Errors are native gRPC statuses, never message payloads.

## Constants

- `MAX_MESSAGE_SIZE` (16 MiB): cap on a single encoded gRPC message.
- `CHUNK_SIZE` (1 MiB): one slice of a data-plane transfer.
- `MAX_TRANSFER_SIZE` (8 GiB): reassembly guard for one transfer.

## Feature flags

The default build is the contract alone: schemas, generated types, and the constants.

- `server` — the BPA-side bridges: one `ServiceImpl` per surface, serving the wire over any `hardy_bpa::bpa::BpaRegistration` for a host to mount on its own tonic transport.
- `client` — the component SDK: `BpaClient` registers local component implementations against a remote BPA over the v1 wire.
- `instrument` — enables `tracing` span instrumentation.
