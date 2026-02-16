# hardy-proto Design

gRPC protocol definitions and Rust proxy implementations for distributed BPA deployment.

## Design Goals

- **Multi-process deployment.** Enable CLAs and application services to run as separate processes from the BPA, communicating over gRPC. This allows independent scaling, fault isolation, and deployment flexibility.

- **Language-agnostic interface.** The protobuf specifications serve as an open interface that can be implemented in any language with gRPC support. External systems can integrate with Hardy without using Rust.

- **Transparent proxying.** Rust clients use the same traits (`Cla`, `Sink`, `Application`, `Service`) whether communicating in-process or over gRPC. The proxy layer handles protocol translation invisibly.

## Architecture Overview

The package provides two main components:

```
┌─────────────────────────────────────────────────────────────┐
│  Proto Definitions (*.proto)                                │
│  ├── cla.proto      - CLA ↔ BPA bidirectional streaming     │
│  └── service.proto  - Application/Service ↔ BPA streaming   │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  Rust Proxy Module (proxy/)                                 │
│  ├── cla.rs         - Implements hardy_bpa::cla::{Cla,Sink} │
│  ├── application.rs - Implements services::Application      │
│  └── service.rs     - Implements services::Service          │
└─────────────────────────────────────────────────────────────┘
```

Both CLA and service protocols use bidirectional streaming with message correlation, enabling asynchronous request/response patterns over a single gRPC stream.

## Key Design Decisions

### Bidirectional Streaming with Message Correlation

Rather than separate RPC calls for each operation, both protocols use a single bidirectional stream per connection. The stream is established via a `Register()` RPC and remains open for the session lifetime.

**Stream message structure:**

```protobuf
message ClaToBpa {
  uint32 msg_id = 1;           // Correlation ID
  oneof msg {
    google.rpc.Status status = 2;  // Error response
    RegisterClaRequest register = 3;
    DispatchBundleRequest dispatch = 4;
    // ... other message types
  }
}
```

Each direction has a wrapper message containing:

- `msg_id` - Correlation identifier for request/response matching
- `oneof msg` - The actual payload, one of several message types

**Protocol flow:**

```
Client                                    Server
   │                                         │
   │─── Register() RPC ─────────────────────>│
   │<══════════ Bidirectional Stream ═══════>│
   │                                         │
   │─── RegisterRequest (msg_id=0) ─────────>│
   │<── RegisterResponse (msg_id=0) ─────────│
   │                                         │
   │─── DispatchBundle (msg_id=1) ──────────>│  Client-initiated
   │─── AddPeer (msg_id=2) ─────────────────>│  (concurrent)
   │<── DispatchResponse (msg_id=1) ─────────│
   │<── AddPeerResponse (msg_id=2) ──────────│
   │                                         │
   │<── ForwardBundleRequest (msg_id=3) ─────│  Server-initiated
   │─── ForwardBundleResponse (msg_id=3) ───>│
   │                                         │
```

The first message must always be a registration request with `msg_id=0`. After registration succeeds, either side can initiate messages. The sender assigns a unique `msg_id`; the receiver echoes it in the response, allowing the sender to match responses to requests even when multiple operations are in flight.

This design reduces connection overhead, enables server-initiated messages (like forwarding requests from BPA to CLA) without polling, and supports concurrent operations on a single stream.

**Mapping to Component/Sink traits:**

The bidirectional stream directly mirrors the BPA's Component/Sink trait pattern (see [BPA design](../../bpa/docs/design.md#component-registry-and-sink-pattern)):

| Direction | In-Process | Over gRPC |
|-----------|------------|-----------|
| BPA → Component | Component trait methods (`on_register`, `forward`) | Server-initiated stream messages |
| Component → BPA | Sink trait methods (`dispatch`, `add_peer`) | Client-initiated stream messages |

This symmetry allows the proxy module to implement the same traits used for in-process components, making deployment topology transparent to the component implementation.

### Interfaces Not Exposed via gRPC

Two BPA interfaces are intentionally kept in-process only:

**Filter interface**: Filters run in the bundle processing hot path. The latency of gRPC serialization would impact throughput unacceptably. Filters must be compiled into the BPA process.

**Storage interface**: Storage backends that need remote access (PostgreSQL, S3) already provide their own protocols. Adding a gRPC layer would introduce unnecessary overhead. Storage implementations link directly into the BPA.

### Two Service API Levels

The service protocol exposes both the Application API (payload-only) and Service API (full bundle access) described in the [BPA design](../../bpa/docs/design.md). The gRPC messages mirror the trait method signatures, with the BPA validating all service-constructed bundles as a security boundary.

### Trust Model

The gRPC layer is the **security boundary** for the BPA. Two deployment modes exist:

**In-process components** (CLAs, services, filters compiled into the BPA) are fully trusted. They share the same process—if compromised, the entire BPA is compromised. No authorization checks are performed on in-process calls.

**Remote components** (connecting via gRPC) are authenticated and authorized at the gRPC layer:

```
┌─────────────────────────────────────────────────────────────────┐
│  bpa-server process                                             │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  bpa (core) - trusts all callers                          │  │
│  └───────────────────────────────────────────────────────────┘  │
│                              ▲                                  │
│                              │ (direct Rust calls)              │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │  gRPC handlers ◄── TRUST BOUNDARY                         │  │
│  │  ├─ mTLS authentication (certificate = identity)          │  │
│  │  ├─ Namespace validation at registration                  │  │
│  │  └─ Policy enforcement (rate limits, quotas)              │  │
│  └───────────────────────────────────────────────────────────┘  │
│                              ▲                                  │
│                              │ gRPC + mTLS                      │
└──────────────────────────────┼──────────────────────────────────┘
                               │
                    Remote CLA / Service / App
```

**Resource ownership** is enforced structurally by the Sink pattern (see [BPA design](../../bpa/docs/design.md#authorization-and-ownership)). Each gRPC connection receives a Sink bound to its own resources—a client cannot affect another client's registrations because it has no reference to them.

**Security layers** (when mTLS is enabled):

1. **Authentication**: Client certificate required; CN/SAN establishes identity
2. **Registration validation**: Namespace checks on requested EIDs
3. **Ownership enforcement**: Structural via Sink pattern (no token needed)
4. **Policy enforcement**: Rate limits and quotas per connection

The `bpa/` crate remains security-agnostic—all authorization logic lives in `bpa-server/src/grpc/`.

### Error Handling via google.rpc.Status

Errors are embedded in the stream as `google.rpc.Status` messages rather than terminating the stream. This allows granular error reporting for individual operations. Fatal errors (like registration failure) close the stream.

## Protocol Definitions

The `.proto` files define the wire format for each interface:

- **`cla.proto`** - CLA registration, bundle dispatch/forwarding, peer management, and unregistration. Maps to the `Cla` and `cla::Sink` traits.

- **`service.proto`** - Endpoint registration, send/receive, status notifications, cancellation, and unregistration. Defines separate message types for Application API (payload-only) and Service API (full bundle). Maps to the `Application`, `Service`, and corresponding Sink traits.

## Proxy Module

The `proxy` module provides Rust implementations of BPA traits that communicate over gRPC:

- `register_cla()` - Connect a CLA implementation to a remote BPA
- `register_application_service()` - Connect an Application to a remote BPA
- `register_endpoint_service()` - Connect a Service to a remote BPA

Internal traits abstract over message handling:

- `SendMsg` - Compose messages with correlation IDs
- `RecvMsg` - Extract message content and handle status errors
- `ProxyHandler` - Handle incoming notifications and manage lifecycle

The `RpcProxy` struct manages the bidirectional stream, correlating requests with responses via a pending acknowledgement map.

## Integration

### With hardy-bpa

The BPA library defines the traits (`Cla`, `Sink`, `Application`, `Service`). hardy-proto provides gRPC-based implementations that proxy method calls over the network.

### With hardy-bpa-server

The server implements the gRPC service handlers, translating between protobuf messages and BPA trait calls. It manages stream lifecycle, connection authentication, and error propagation.

### With External Clients

Any gRPC client can implement these protocols. Python, Go, or C++ applications can register as services or CLAs without depending on Rust code. The `.proto` files serve as the authoritative interface specification.

## Dependencies

| Crate | Purpose |
|-------|---------|
| hardy-bpa | Trait definitions being proxied |
| hardy-bpv7 | EID and bundle types |
| tonic | gRPC server/client framework |
| prost | Protocol buffer serialization |
| tokio-stream | Async stream utilities |

## Testing

- [Component Test Plan](component_test_plan.md) - gRPC streaming interface verification
