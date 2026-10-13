# hardy-qbcl Design

A convergence layer that carries bundles over QUIC, per draft-ek-dtn-qubicle.

## Design Goals

- **Delegate transport to QUIC.** Qubicle has no in-band handshake, segmentation, acknowledgement, or keepalive of its own: a peer's Node ID comes from its TLS certificate, a bundle is one unidirectional stream, success is the peer's acknowledgement of the stream's data and FIN, and flow control is QUIC's. The crate's own protocol logic is therefore small, and it is kept separate from the QUIC library so that it can be tested without one.
- **Zero-copy receive.** Bundle octets arrive as QUIC stream chunks; the framing layer passes them through as slices of those chunks rather than reassembling them, so the receive path into the BPA costs no copy beyond what the QUIC library already made.
- **Early policy decisions.** The wrapped form exists so that a receiver can learn a bundle's size and version from the first few octets and refuse it before the bulk arrives (Section 4.3.2). The reader reports what it knows as soon as it knows it.

## Architecture Overview

The crate holds:

- `stream` — the stream content framing of Section 4.3.2: a `StreamReader` that is fed each chunk of a receive stream and checks it against Table 1, and `wrapped_head` for the sender.
- `error_code` — the Section 5 application error codes and the rules for reading received codes.
- `node_id` — reading NODE-IDs from a DER certificate and checking them against a configured Node ID (Section 4.2).
- `tls` — the public `Tls` material: trust anchors, the node's identity, and the rustls configurations for both handshake roles, with certificate verifiers that add Node ID authentication to chain validation.
- `endpoint` — a quinn endpoint on one UDP socket that accepts and dials sessions, each carrying the peer's authenticated Node IDs.
- `reliable` — the reliable service of Section 4.3: a `BundleSender` that writes one bundle to its own unidirectional stream and waits for the transfer's outcome, and a `BundleReceiver` that reads one stream through a `StreamReader`.
- `datagram` — the unreliable service of Section 4.4: a `DatagramSender` and `DatagramReceiver` pair over a session, each DATAGRAM frame one BTP-U link-layer PDU.
- `cla` — the public `Qbcl` adapter, which implements the `hardy-bpa` `Cla` trait on the reliable service: a session registry, on-demand dialing, and dispatch of each arriving stream to the BPA.

Per-peer transport configuration (Section 7.2) and the adapter's use of the unreliable service are not yet written.

## Key Design Decisions

### Incremental Reader Instead of a Parser

QUIC delivers a stream as chunks of arbitrary size, and the head of a wrapped bundle (up to nine octets plus the content's first octet) can be split across any of them. Rather than buffer until a parse succeeds, `StreamReader` is a small state machine that consumes framing octets from the front of each chunk and returns the remainder as bundle data. The head is copied into a fixed nine-octet array; nothing else is copied. Every state transition is driven by the data itself, so the reader is identical whether a stream arrives in one chunk or one octet at a time, which the tests check exhaustively near the head.

Framing faults are sticky: once a push fails, every later call returns the same error, so a caller that keeps feeding a doomed stream cannot get a different answer.

### Hand-Rolled Byte String Head

The only CBOR the framing needs is the head of a definite-length byte string, a first octet in 0x40–0x5b followed by 0, 1, 2, 4, or 8 length octets. `hardy-cbor`'s general head parser would also accept tags and every other major type, and its error paths do not map onto Table 1's cases. The crate decodes and encodes that one head directly and uses `hardy-cbor` only as a test oracle, checking both directions against it at every boundary between head sizes.

### Recognizing a Version Is Not Supporting It

The reader reports a BPv6 bundle as readily as a BPv7 one, because Table 1 makes both first octets valid framing. Whether to accept BPv6 is local policy, and a receiver that declines cancels the transfer with `QBCL_UNSUPPORTED_BUNDLE_VERSION` (Section 4.3.2); treating it as a framing fault would close the whole connection instead. The same separation applies to the direct form, which is valid framing that a receiver may decline with `QBCL_LENGTH_REQUIRED`.

### Reading Received Error Codes

Section 5 gives different rules for codes that are unrecognized, or out of place, depending on the frame: on CONNECTION_CLOSE such a code reads as `QBCL_PROTOCOL_ERROR`, and on RESET_STREAM or STOP_SENDING, where `QBCL_NO_ERROR` must not appear, it reads as `QBCL_TRANSFER_CANCELLED`. The two rules are separate constructors on `ErrorCode`, so the session layer chooses the rule by naming the frame, and the raw value never reaches transfer outcome logic.

### Node ID Authentication in the Certificate Verifier

Section 4.2 makes a Node ID mismatch a certificate validation failure that terminates the handshake, so the check runs inside the rustls verifiers rather than after the session is up. On the dialing side a client configuration is built per dial, carrying the Node ID configured for that peer, if any; on the accepting side there is no configured Node ID yet, so the dialing peer's certificate must carry a NODE-ID. A failure is returned as `CertificateError::NotValidForName`, which rustls sends as the bad_certificate alert RFC 9174 Section 4.4.4.1 asks for; the specific reason goes to the log. Once the handshake completes, the session reads the accepted Node IDs back from the peer's certificate.

Chain validation uses webpki through rustls, with the trust anchors loaded from a directory of PEM files as in `hardy-tcpclv4`. Host names and IP addresses in the certificate are not checked: Section 4.2 identifies a peer by its Node ID alone, which is the policy RFC 9174 Section 4.4.5 recommends. webpki still applies extended key usage, so a certificate that lists any EKU must include id-kp-serverAuth to be accepted from a listener and id-kp-clientAuth to be accepted from a dialer; a certificate with no EKU extension is accepted in both roles.

Session resumption is disabled in both roles. A resumed handshake does not run the certificate verifier, so it would establish a session without authenticating a Node ID; without resumption there is also no 0-RTT, which Section 6.4 discourages for bundles anyway.

### Padding Every Datagram

Every UDP datagram carrying 1-RTT packets is padded to the path MTU, and path MTU discovery is disabled, so the MTU stays at the 1200 octets that RFC 9000 Section 14 requires every path to support. An observer then sees datagrams of one size whatever the bundles, acknowledgements, and BTP-U messages inside them. The cost is bandwidth for padding and a 1200-octet ceiling on packets, which limits throughput on fast links; making both configurable per peer is left for the per-peer transport configuration of Section 7.2. Initial packets are already padded to 1200 octets by the handshake, and the Handshake-space packets of the server's first flight are not padded.

### The Unreliable Service

The service opens only if both peers advertised `max_datagram_frame_size` (Section 4.4). quinn lets a side send datagrams when only the peer advertised, so the session also remembers whether its own endpoint offered the service and refuses to open it otherwise. An endpoint that does not offer it advertises no `max_datagram_frame_size` at all.

The BTP-U PDU size is quinn's `max_datagram_size()` when the service opens. With path MTU discovery off it does not change during the session. BTP-U uses variable-length link PDUs and message framing, and does no padding of its own: QUIC pads the packet around the DATAGRAM frame. One `Config` sets the transfer window for both directions, since the window must agree at both peers. The sender's initial transfer number comes from the operating system's RNG, so that a peer is unlikely to take a new session's transfers for a previous one's.

The sender writes each PDU into a fresh buffer with `next_pdu_write` and hands it to `send_datagram_wait`, which waits for room in quinn's datagram queue. `send_datagram` would instead discard the oldest queued datagrams, losing segments the sender has already counted as sent. A bundle is reported once its last octet has been handed to QUIC; that is all the unreliable service can say of its outcome. The vectored writer, which would avoid copying bundle octets into the PDU buffer, is deferred.

### The Reliable Service

A sender always uses the wrapped form, because it always knows the bundle's length, and the wrapped form lets the receiver refuse an oversized bundle from its head. `BundleSender::finish` sends the FIN and awaits quinn's `SendStream::stopped()`: `Ok(None)` means the peer's QUIC endpoint has acknowledged every octet and the FIN, which is the success Section 4.3.1 defines; STOP_SENDING and a lost connection are failures. That is receipt by the peer's transport, not acceptance by its BPA; see the draft feedback on transfer completion.

Both ends override a quinn default on drop. A dropped `SendStream` finishes the stream, which would present an abandoned transfer as complete, so a `BundleSender` dropped before `finish` resets it with `QBCL_TRANSFER_CANCELLED`. A dropped `RecvStream` sends STOP_SENDING with code 0, which is `QBCL_NO_ERROR` and forbidden there (Section 5), so a `BundleReceiver` dropped before the end of its stream stops it with `QBCL_TRANSFER_CANCELLED` instead. The sender also checks the octets written against the declared length, cancelling a transfer that would run over or finish short.

A framing fault on a received stream closes the connection with `QBCL_PROTOCOL_ERROR` before the error is returned (Section 4.3.2). quinn returns streams that had fully arrived before a connection closed ahead of the close itself, so a receiver drains them and delivers their bundles even after the sender has closed.

### The Convergence Layer Adapter

`Qbcl` binds one endpoint, which both accepts and dials. A session dialed from that socket arrives at the peer from the address the dialer listens on, so the peer's `ClaAddress::Quic` is the same whichever side dialed, and one registry keyed by address serves both. A forward to an address with no open session dials one; a per-address lock makes concurrent forwards share the dial. Each session reports its peer to the BPA with the Node IDs from the peer's certificate, and removes it when the session ends. A session that ends by idle timeout (Section 4.5) therefore removes the peer too, and an embedder that wants a standing session calls `Qbcl::connect` again. When two sessions with one peer exist, the newer serves forwards, and only the session still registered removes the peer when it closes.

`forward` opens a stream for the bundle and writes each segment as the BPA hands it over, so QUIC flow control holds the BPA back and no bundle is buffered whole. Once the last segment is written, it spawns a task that awaits the outcome and reports it with `transfer_outcome`, and answers `Accepted`. Failures before that point are answered at once. A dial or stream that cannot open, or a session lost mid-write, is `NoNeighbour`, since it concerns the link. A producer that goes away, delivers the wrong length, or is cancelled by the peer mid-write is an error, since it concerns one bundle. A peer cancellation normally arrives this way, because a cancelled bundle larger than the stream window leaves the sender waiting for credit.

Each arriving stream gets its own task, which hands the BPA a `Receiver<Segment>` reading straight from the stream. As the octets arrive, it applies the effective maximum bundle size from registration (to the wrapped form's declared length, or to the count so far) and refuses BPv6. A refused transfer is stopped with `QBCL_BUNDLE_TOO_LARGE` or `QBCL_UNSUPPORTED_BUNDLE_VERSION`, and the pull fails so the BPA discards what it has. The BPA's verdict is then applied as far as Section 4.6 allows:

- **Accepted** before the end of the stream (a duplicate recognized from its headers, say): the rest is read and discarded, so that the sender sees the transfer complete. Stopping the stream would report a failure and invite a needless retransmission.
- **Refused** while the stream is still arriving: stopped with `QBCL_TRANSFER_CANCELLED`.
- **Refused** after the FIN: the sender has already seen success, so the refusal can only be logged.

The unreliable service is not offered yet: using it needs a BPA policy that chooses a service per bundle.

## Integration

- **quinn** — QUIC, with rustls on the aws-lc-rs provider. TLS 1.3 only, ALPN `qbcl`, and no bidirectional streams (Section 4.3.3).
- **hardy-bpa** — `Qbcl` implements `Cla`, registered with `Qbcl::cla_init()`: the QUIC address type, no lane limit (every transfer has its own stream), and the configured maximum bundle size. It reports peers by the Node IDs from their certificates and streams bundles in both directions without buffering them whole.
- **hardy-btpu** — the unreliable service (Section 4.4) carries BTP-U messages in QUIC DATAGRAM frames, one link-layer PDU per frame.

## Standards Compliance

The crate implements draft-ek-dtn-qubicle-02. Notable interpretations:

- **An empty stream is a protocol error.** Table 1 dispatches on a first octet that a stream ending with no data does not have; such a stream carries no bundle, and the reader reports it as a framing fault.
- **A longer-than-shortest head is accepted.** RFC 8949 permits a byte string length in more octets than needed, and Section 4.3.2 does not require the shortest encoding, so the reader accepts any of the five head sizes. The sender always writes the shortest.
- **An empty byte string is a protocol error,** as content that does not begin with a recognized bundle version octet.
- **Cancellation is advisory.** A receiver that stops a stream whose data and FIN have all arrived cannot reach the sender, which reports success. This includes an oversized wrapped-form bundle that fits within the stream window, which can arrive in full before the receiver reads its head. The draft feedback (R3) proposes a fix.

## Testing

Unit tests sit beside the code they test. The framing tests cover each Table 1 case, every error, chunk boundaries at every position near the head, zero-copy pass-through, and the wrapped head against `hardy-cbor`; the error code tests check Table 2 and both receive rules against values in each IANA range.

The Node ID tests build certificates with rcgen, hand-encoding the subjectAltName because rcgen writes otherName values only as UTF8String. The endpoint tests run real handshakes between two endpoints on loopback, with a test CA writing PEM files to a temporary directory and fresh keys for every certificate. They cover Node IDs learned in both directions, configured Node IDs that match, mismatch, or stand in, certificates without a NODE-ID refused on either side, an untrusted CA, and the padding, checked from the accepting side's UDP statistics. The datagram tests send a bundle that fits one datagram, a 5000-octet bundle that must be segmented and reassembled, bundles in both directions at once, and check that the service does not open on either side when only one peer offers it.

The reliable service tests cover a bundle arriving whole and acknowledged, cancellation by the receiver with its code, both drop overrides, the sender's length checks, a framing fault closing the connection, a bundle acknowledged before the sender closes still being received, and a transfer failing when the peer closes. The adapter tests register two `Qbcl` instances with a mock `Sink` and check forwarding and dispatch end to end, peer addition and removal, a shared dial, early acceptance completing at the sender, refusal and the size and version policies cancelling with their codes, truncated and short producers, and a dial refused for a configured Node ID the peer lacks. Bundles that a test needs cancelled mid-transfer are larger than quinn's stream window, so the outcome does not depend on timing.
