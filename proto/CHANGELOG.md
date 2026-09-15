# Changelog

All notable changes to `hardy-proto` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0]

### Changed
- **BREAKING**: the wire protocol is redesigned as a versioned v1 contract with no interoperability with the previous protocol: one gRPC API per kind of component (`hardy.application.v1`, `hardy.service.v1`, `hardy.cla.v1`, `hardy.routing.v1`) over one shared schema (`hardy.common.v1`). `Subscribe` carries the registration up and a pure event stream down, every other RPC presents the session token minted at registration, and payload bytes move on chunked streaming calls that the client cancels in-band and the BPA abandons by ending the call with a status.
- **BREAKING**: the crate core is reduced to the contract: the generated types (one root module per package: `common`, `application`, `service`, `cla`, `routing`), the wire constants (`MAX_MESSAGE_SIZE`, `chunking::DEFAULT_CHUNK_SIZE`, `chunking::MAX_CHUNK_SIZE`, `DEFAULT_MAX_FRAME_SIZE`, `MAX_TRANSFER_SIZE`), and the domain conversions. The previous client, server, and proxy implementation (`RemoteBpa`, `GrpcServer`, `RpcProxy`) is removed, as is the vendored `google/rpc/status.proto`.
- The default (no-feature) build depends on `prost`, `prost-types`, `tonic` and `tonic-prost` alone; `hardy-bpa`, `hardy-bpv7`, `thiserror`, `tokio` and `tracing` are compiled in only by the `client` and `server` features, which also gate the domain conversions, and only `server` pulls `dashmap`, `foldhash` and `rand`.
- Errors are gRPC statuses, never message payloads: a code and a short hand-written message, with no details and nothing a client sent reflected back. The comments in each `.proto` name the codes a call can end with, and a code they do not name for a call came from the transport.
- The schemas speak RFC 9171 vocabulary (ADU, delivery, bundle status report assertions, transmission flags), and the RFC 9171 bundle id is the one identity across announcements, collection, send results, and status reports.
- The route-action conversions refuse RFC 9171's reserved status-report reason code 255 in both directions: inbound it is `INVALID_ARGUMENT` at the server, outbound the client SDK's sink refuses it before the wire, and a status report announcing it is dropped by the SDK.

### Added
- The `server` feature: `ApplicationServiceImpl`, `ServiceServiceImpl`, `ClaServiceImpl` and `RoutingServiceImpl`, each serving its API over any `hardy_bpa::bpa::BpaRegistration`, with `into_server()` wrapping it in the generated tonic server sized for the wire contract.
- Session tokens are random, server-minted bearer values that the session index alone makes valid; a dead or forged token is `UNAUTHENTICATED`. The generated messages that carry `session_token` print its length and never its bytes under `Debug`.
- A failed registration fails the `Subscribe` call itself; a commenced one runs on a pool task the RPC cannot cancel, so an abandoned call either never registers or is unregistered on the spot. The `Registration` event precedes anything the BPA announced from inside registration, and a session's final status has a slot reserved on the event channel that a non-reading client cannot consume.
- `server::Limits`: the per-API bounds on stalled clients, replaceable through each `*ServiceImpl::with_limits`. The handshake (10 s) bounds a call's first message; `idle` (30 s) bounds every wait in a live session; `claim` (30 s) bounds an announced bundle's wait for its collecting call; `grace` (30 s) and `min_rate` (1024 B/s) hold every transfer to a minimum rate; `max_sessions` (64) caps live sessions, refusing the next `Subscribe` with `RESOURCE_EXHAUSTED`. A stall closes the session with `DEADLINE_EXCEEDED` and the BPA keeps the bundle.
- `server::MAX_INBOUND_TRANSFERS` (4) bounds the `Send` or `Dispatch` calls one session has open at once; a further call waits its turn. `server::SESSION_FOOTPRINT` states the most a stalled application or service session holds.
- `hardy.common.v1.Sizes`, announced in every `Registration`: the message cap, the transfer cap, and the chunk size the session runs at, which is `min(DEFAULT_CHUNK_SIZE, Register.max_chunk_size)`; an ask below 1 KiB is `INVALID_ARGUMENT`.
- A binding declared transfer size: `SendMetadata.adu_size` on the application API and `bundle_size` on the service `Send` and CLA `Dispatch`. A declaration above `MAX_TRANSFER_SIZE`, or above the bundle size limit agreed for a CLA registration, is `RESOURCE_EXHAUSTED` before any byte moves; a transfer that ends at a different size is `INVALID_ARGUMENT`. The server never allocates against a declaration.
- `cla::AddressError`: the focused error a wire `ClaAddress` conversion returns (previously `tonic::Status` from the transport-agnostic contract layer), and `routing::RouteActionError` for the route-action conversions.
- The `client` feature: `BpaClient`, registering local `Application`, `Service`, `Cla` and `RoutingAgent` implementations against a remote BPA. `new` and `with_connections` apply the `default_endpoint` settings (HTTP/2 keep-alive, adaptive flow-control window, chunk-sized frames); `with_endpoint` and `with_endpoint_connections` take an `Endpoint` as configured. A registration takes the next connection in turn and keeps every call of its session on it, so several connections spread sessions rather than one session's calls.
- `RegistrationHandle<Identity, E>`, returned by every `register_*`, carries the identity the BPA assigned and resolves when the session ends, by `join()` or by being awaited: `Ok(())` for a clean end of stream, which the server sends for an ending the client asked for, and the component's error otherwise.
- The SDK maps a status to the component's error by its code alone: `UNAUTHENTICATED`, `UNAVAILABLE` and `DEADLINE_EXCEEDED` are `Disconnected`; on a data-plane call `CANCELLED` and `ABORTED` are `StreamCancelled`, `ALREADY_EXISTS` is `DuplicateBundle` and `FAILED_PRECONDITION` is `Dropped`; at registration `ALREADY_EXISTS` is `ServiceIdInUse` or `AlreadyExists` and `FAILED_PRECONDITION` is `NoIpnNodeId` or `NoDtnNodeId` by the scheme asked for; everything else is `Internal` carrying the status.

## [0.2.0]

### Added
- Public `MAX_MESSAGE_SIZE` (16 MiB) and `MAX_PAYLOAD_SIZE` constants bounding gRPC message and payload sizes; sinks pre-check payload size against `MAX_PAYLOAD_SIZE` before sending.

### Changed
- **BREAKING:** replaced the `server::init()` free function with a `GrpcServer` struct — `GrpcServer::new()` builds it, `GrpcServer::serve(cancel)` returns a future the caller spawns/awaits — giving callers explicit control of the serve lifecycle.
- **BREAKING:** tracked the upstream `hardy_bpa::routes` → `hardy_bpa::routing` rename (`RemoteBpa`'s `BpaRegistration` impl, route action/error/sink types).
- Raised the minimum supported Rust version (MSRV) to 1.95.

### Fixed
- Map routing validation errors to appropriate gRPC status codes (`invalid_argument` for null/own-node next hops, `unavailable` for disconnects, `internal` otherwise) instead of always surfacing as internal errors.
- Pre-check payload size before sending so an over-sized bundle returns a typed error instead of breaking the underlying gRPC stream.
- RpcProxy concurrent-delivery correctness: the reader is now a pure demultiplexer and request ids are drawn per-side, so concurrent request/reply traffic on a single stream can no longer deadlock the reader or mis-route replies.
- Harden receive-path error handling and propagate failures instead of swallowing them.

### Removed
- `server::init()` (superseded by `GrpcServer`).

Releases before this version predate this changelog; see the git history for details.
