# hardy-tcpclv4 Design

TCP Convergence Layer Protocol Version 4 implementation for DTN bundle transport.

## Design Goals

- **RFC 9174 compliance.** Implement the complete TCPCLv4 specification including contact negotiation, session establishment, bidirectional transfer, and graceful termination.

- **High concurrency.** Handle many simultaneous connections efficiently. Each session runs as an independent async task, preventing slow transfers or network delays from blocking other sessions.

- **Connection reuse.** TCP, TLS, and TCPCLv4 handshakes are expensive. The implementation maintains a pool of idle connections per peer address, reusing established sessions when forwarding bundles to known peers.

- **TLS by default.** RFC 9174 Section 7.11 mandates TLS as mandatory-to-implement. The implementation requires TLS unless explicitly disabled by configuration.

## Architecture

The implementation follows the RFC's conceptual separation between TCPCL entities, sessions, and transfer streams:

```
TCPCL Entity (Cla struct)
    │
    ├── Passive Listener (listen.rs)
    │       └── Accepts incoming TCP connections
    │
    ├── Active Connector (connect.rs)
    │       └── Initiates outbound TCP connections
    │
    └── Connection Registry (connection.rs)
            │
            └── Connection Pool (per peer address)
                    │
                    └── Session Tasks (session.rs)
                            └── Transfer Streams (bidirectional)
```

### Layered Design

The code separates concerns across four layers:

**CLA Interface Layer** (`lib.rs`, `cla.rs`): Implements the `hardy_bpa::cla::Cla` trait. Receives forwarding requests from the BPA and manages the overall CLA lifecycle.

**Connection Management Layer** (`connection.rs`): Maps peer addresses to connection pools. Handles connection reuse decisions and peer registration with the BPA.

**Session Layer** (`session.rs`, `connect.rs`, `listen.rs`, `transport.rs`): Manages the TCPCLv4 session lifecycle from contact exchange through termination. Each session runs as an isolated async task.

**Codec Layer** (`codec.rs`): Encodes and decodes TCPCLv4 messages using tokio-util's framed I/O.

## Key Design Decisions

### Implicit Session State Machine

RFC 9174 Section 3.1 defines session states: Connecting, Contact Negotiating, Session Negotiating, Established, Ending, Terminated, and Failed. Rather than implementing an explicit state enum, the implementation represents states implicitly through code flow.

The contact exchange and session initialization occur in `Connector::connect()` and `Listener::new_contact()`. Once complete, control transfers to `Session::run()` which handles the Established state. Termination is handled by `Session::shutdown()` and `Session::on_terminate()`.

This approach was chosen because the state transitions are linear during setup, and the Established state requires different handling (bidirectional transfer loop) that maps naturally to a separate function.

### Connection Pooling Strategy

Each `ConnectionPool` maintains separate sets of idle and active connections. When forwarding a bundle:

1. Try an idle connection first, moving it to the active set
2. If no idle connections exist and the pool isn't at capacity, signal the caller to establish a new connection
3. If at capacity, queue to a random active connection

This balances connection reuse against parallelism. The `max_idle_connections` configuration (default: 6) limits memory usage from idle connections while allowing burst capacity.

### Tower Service for Listener

The TCP listener wraps connection acceptance as a Tower `Service`, enabling middleware composition:

```rust
tower::ServiceBuilder::new()
    .rate_limit(1024, Duration::from_secs(1))
    .service(ListenerService::new(listener))
```

This provides connection flood protection without modifying the core acceptance logic. Future security layers (IP blocking, authentication gates) can be composed as additional middleware.

### TLS Integration

TLS uses rustls with tokio-rustls for async operation. The contact header exchange (RFC 9174 Section 4.2) includes a CAN_TLS flag; if both peers indicate TLS support, the TLS handshake occurs before session initialization.

Server certificate validation supports three modes for the server name:
1. Configured server name (for certificates issued to domain names)
2. "localhost" for loopback connections
3. IP address (may fail if certificate is domain-issued)

A debug option allows accepting self-signed certificates for testing, with prominent warnings.

### TCPCLv3 Interoperability

When connecting to a peer that responds with protocol version 3, the implementation sends a TCPCLv3 SHUTDOWN message (`0x45, 0x01`) before closing. This allows legacy peers to clean up gracefully rather than interpreting the disconnect as an error.

## Session Lifecycle

Following RFC 9174 Section 3.2, session establishment proceeds through:

1. **TCP Connection**: Active entity initiates, passive entity accepts
2. **Contact Exchange**: Both peers send the 6-byte contact header ("dtn!" magic, version 4, flags)
3. **TLS Negotiation**: If both peers indicated TLS support, perform TLS handshake
4. **Session Initialization**: Exchange SESS_INIT messages with keepalive interval, segment/transfer MRU, and node ID
5. **Established**: Bidirectional bundle transfer via XFER_SEGMENT/XFER_ACK

Session termination follows RFC 9174 Section 6: send SESS_TERM, continue receiving, await SESS_TERM reply, close. The implementation handles "terminations passing in the night" where both peers initiate termination simultaneously.

## Transfer Protocol

Large bundles are segmented per RFC 9174 Section 5.2.2, respecting the peer's Segment MRU (maximum receive unit). Each segment receives an acknowledgement (XFER_ACK). Transfer IDs are per-session counters; if exhaustion is imminent, the session terminates with ResourceExhaustion rather than risk ID reuse.

Peers may refuse transfers (XFER_REFUSE) for reasons including: already received (Completed), temporary overload (NoResources), or session ending (SessionTerminating). The implementation handles Retransmit by resending the bundle.

## Configuration

| Option | Default | Description |
|--------|---------|-------------|
| `address` | `[::]:4556` | Listen address (RFC 9174 Section 8.1 assigns port 4556) |
| `segment_mru` | 16384 | Maximum segment payload size to receive |
| `transfer_mru` | 1GB | Maximum total bundle size to receive (assembled in memory) |
| `max_idle_connections` | 6 | Maximum idle connections per peer address |
| `connection_rate_limit` | 64 | Maximum incoming connections per second |
| `contact_timeout` | 15 | Seconds to wait for contact header |
| `keepalive_interval` | 60 | Keepalive interval in seconds (None to disable) |
| `must_use_tls` | true | Require TLS for all connections |

RFC 9174 timing recommendations are enforced via warnings:
- Contact timeout SHOULD NOT exceed 60 seconds (Section 4.3)
- Keepalive interval SHOULD NOT exceed 600 seconds (Section 5.1.1)

## Integration

### With hardy-bpa

Implements `hardy_bpa::cla::Cla` trait. The BPA provides a `Sink` for dispatching received bundles and registering discovered peers. When a session learns the peer's node ID during SESS_INIT, it registers the peer via `sink.add_peer()`.

### With hardy-bpa-server

When compiled with the `tcpclv4` feature, the BPA server runs TCPCLv4 in-process without gRPC overhead.

### With hardy-tcpclv4-server

A standalone application linking this library with hardy-proto for gRPC connectivity to a remote BPA instance.

## Dependencies

| Crate | Purpose |
|-------|---------|
| hardy-bpa | CLA trait definition |
| tokio, tokio-rustls | Async runtime and TLS |
| tokio-util | Framed codec I/O |
| rustls, rustls-pemfile | TLS implementation and certificate parsing |
| tower | Service pattern for listener middleware |

## Future Work

- **Mutual TLS (mTLS)**: Client certificate authentication is not yet implemented
- **Session Extensions**: Currently rejects all critical extensions per RFC 9174 Section 4.8
- **Transfer Extensions**: No transfer extensions have been published as of RFC 9174; support can be added when specifications emerge

## Standards Compliance

- [RFC 9174](https://www.rfc-editor.org/rfc/rfc9174.html) - Delay-Tolerant Networking TCP Convergence-Layer Protocol Version 4

## Testing

- [Test Plan](test_plan.md) - Session lifecycle and protocol message handling
