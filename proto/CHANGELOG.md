# Changelog

All notable changes to `hardy-proto` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- **BREAKING**: the wire protocol is redesigned as a versioned v1 contract, with no interoperability with the previous protocol. One gRPC service per component surface (`application.v1`, `service.v1`, `cla.v1`, `routing.v1`): `Subscribe` carries the registration lifecycle up and a pure event stream down, every component action is an ordinary RPC gated by the session token minted at registration, and payload bytes move on chunked streaming calls with in-stream cancellation in both directions. The schemas speak RFC 9171 vocabulary (ADU, delivery, bundle status report assertions, transmission flags), and the RFC 9171 bundle id is the one identity across announcements, collection, send results, and status reports. Errors are native gRPC statuses, never message payloads, which removes the vendored `google/rpc/status.proto`.
- **BREAKING**: the crate core is reduced to the contract: the v1 schemas compiled to generated types (one root module per package: `application`, `service`, `cla`, `routing`), the wire constants (`MAX_MESSAGE_SIZE`, `CHUNK_SIZE`, `MAX_TRANSFER_SIZE`), and the domain conversions. The previous client, server, and proxy implementation (`RemoteBpa`, `GrpcServer`, the `RpcProxy` correlation engine) is removed.

### Added
- The `server` feature: the BPA-side bridges (`ApplicationServiceImpl`, `ServiceServiceImpl`, `ClaServiceImpl`, `RoutingAgentServiceImpl`, and the session-token `Signer`), each serving its surface over any `hardy_bpa::bpa::BpaRegistration`, for a host to mount on its own tonic transport.
- The `client` feature: the component SDK. `BpaClient` registers local `Application`/`Service`/`Cla`/`RoutingAgent` implementations against a remote BPA over the v1 wire, carrying the session lifecycle, HTTP/2 keepalive, and the chunked data-plane calls.

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
