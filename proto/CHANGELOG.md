# Changelog

All notable changes to `hardy-proto` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `GrpcServer::local_addr()`: the address the listening socket is bound to. `GrpcServer::new` now binds the socket, so a config address with port 0 gets a kernel-assigned port readable through this accessor, bind failures surface at construction instead of inside `serve`, and connections are accepted into the listener's backlog from construction onward.
- `RegisterClaRequest.max_bundle_size` declares the remote CLA's own receive limit (presence-tracked `optional`: absent = no limit, an explicit 0 is rejected), and `RegisterClaResponse.max_bundle_size` relays back the effective cap — the BPA's dispatch-enforced cap folded with the declaration; an absent response cap means none was negotiated (a BPA predating the field, or no size policy on either side) and maps client-side to `None`, leaving the CLA on its own configured limits. The declaration only shrinks the echoed cap: the BPA does not enforce it at egress and may still offer larger bundles, which the CLA must refuse per-bundle (documented on the field). `RegisterClaRequest.lane_count` declares the remote CLA's lane count with the same presence semantics (absent = no limit, an explicit 0 is rejected), so every `ClaInit` declaration is carried on the wire.
- `DispatchBundleResponse.verdict` (`DispatchVerdict`) carries the BPA's acceptance verdict for a dispatched bundle (accepted = acknowledge the transfer, refused = withhold the acknowledgement); an unspecified/absent verdict (a BPA predating the field, which only responded on acceptance) is read as accepted. The CLA client sink's pre-flight failures (a cancelled stream, the transport cap) are reported as local refusals (`Acceptance::Refused`) instead of typed errors, which are now reserved for proxy faults.
- `ForwardBundleRequest.bundle_id` — the RFC 9171 bundle identifier in key form, identifying the transfer for correlation; CLAs treat it as opaque. Required: the CLA client rejects a forward that omits it, so a CLA built on this crate requires a BPA of at least the same version (the reverse skew — an old CLA against a new BPA — degrades safely).
- Deferred transfer outcomes on the wire: `accepted` as a `ForwardBundleResponse` result, and the `TransferOutcomeRequest`/`TransferOutcomeResponse` pair resolving an accepted transfer as `completed` or `failed` (with an opaque `google.rpc.Status` reason), keyed by `bundle_id`. Deferral is a per-bundle choice in the forward answer — there is no registration-level deferral capability flag.
- `AppReceiveRequest.bundle_id` and `ServiceReceiveRequest.bundle_id` — the delivered bundle's identifier, in the key encoding documented on `SendResponse.bundle_id`. Required: the client SDK fails the delivery without it.

### Changed
- **BREAKING:** `GrpcServer::serve` returns `Result<(), Box<dyn Error + Send + Sync>>` (was `Result<(), tonic::transport::Error>`): serving on the pre-bound listener can also fail on runtime registration, not only in transport.
- **BREAKING:** `RemoteBpa::register_cla` takes the CLA's set-once declarations as a `hardy_bpa::cla::ClaInit` parameter — address type, lane count, and declared receive limit all travel in the registration request — and the client-side relay delivers the negotiated cap to `Cla::on_register` as `Option<NonZeroU64>` (wire absence ↔ `None`).
- **BREAKING** (`serde` feature): the server `Config` refuses unknown keys at deserialization, so a typo in a consumer's `grpc` config section fails loudly instead of silently leaving the default in force.
- **BREAKING:** `ForwardBundleRequest.queue` is renamed to `lane` (same field number and type: binary-compatible on the wire, breaking for generated-code consumers).
- **BREAKING:** tracked the upstream `hardy-bpa` trait rework: the client-side `Application`/`Service` implementations deliver via `on_deliver` with the bundle ID and a segment stream, and the CLA client's `forward` carries the lane, `total_len`, and segment stream.

### Fixed
- The `bundle_id` comments claimed the id is "formatted as specified in RFC 9171" — an encoding that RFC does not define. The actual encoding (base64url without padding over the canonical CBOR array of the id's components) is now documented on `SendResponse.bundle_id` and referenced by every other `bundle_id` field.

### Removed
- **BREAKING:** the `cancel` exchange — `CancelRequest`/`CancelResponse` and the `cancel` member of all four stream oneofs (field number 7 and the name are reserved in each). The implementation was status-blind; see the `hardy-bpa` changelog.
- **BREAKING:** `AppReceiveRequest.source` (field number and name reserved) — the sender's endpoint ID is the `bundle_id`'s source component.

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
