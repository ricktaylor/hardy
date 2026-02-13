# hardy-echo-service Design

In-process echo service library for bundle round-trip testing.

## Design Goals

- **Simple testing.** Provide a minimal service that reflects bundles back to their source, enabling round-trip time measurement and connectivity verification.

- **In-process deployment.** Run within the BPA process for zero IPC overhead. Applications like hardy-bpa-server embed this library directly.

- **Minimal footprint.** Keep the implementation simple - receive a bundle, swap source and destination, send it back.

## Architecture Overview

The echo service implements the BPA's `Service` trait (low-level API):

```
Incoming Bundle                   Echo Service                    Outgoing Bundle
───────────────                   ────────────                    ───────────────
src: ipn:2.1    ──► on_receive() ──► Editor ──► sink.send() ──►   src: ipn:1.7
dst: ipn:1.7        (parse)         (swap)                        dst: ipn:2.1
payload: [...]                                                    payload: [...]
```

The service preserves the original bundle structure, only swapping the source and destination EIDs. All extension blocks and payload remain unchanged.

## Key Design Decisions

### Service Trait vs Application Trait

The echo service implements `Service` (low-level) rather than `Application` (high-level). This provides access to raw bundle bytes, allowing the service to:

- Parse the complete bundle structure
- Use the Editor to modify and rebuild the bundle
- Preserve extension blocks that would be stripped by the Application API

### Editor-Based Bundle Modification

Rather than constructing a new bundle from scratch, the service uses `hardy_bpv7::Editor` to modify the existing bundle in place. This:

- Preserves all extension blocks and flags
- Maintains bundle integrity
- Requires only source/destination changes, not full bundle construction

## Integration

### With hardy-bpa

Implements `hardy_bpa::services::Service`. The BPA calls `on_receive()` when a bundle arrives at the registered endpoint.

### With hardy-bpa-server

When compiled with the `echo` feature, the server instantiates `EchoService` and registers it at a configured endpoint (e.g., `dtn://node/echo` or `ipn:1.7`).

## Dependencies

| Crate | Purpose |
|-------|---------|
| hardy-bpa | Service trait definition |
| hardy-bpv7 | Bundle parsing and Editor |
| hardy-async | `Once` cell for sink storage |
