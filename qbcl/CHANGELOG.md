# Changelog

All notable changes to `hardy-qbcl` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this crate adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Stream content framing per draft-ek-dtn-qubicle-02 Section 4.3.2: an incremental reader that classifies a stream as a direct-form or wrapped-form bundle from its first octets, passes bundle octets through without copying, and reports framing faults as protocol errors; and the shortest-form wrapped head encoder.
- The Section 5 application error codes, with the receive-side rules: an unrecognized CONNECTION_CLOSE code reads as `QBCL_PROTOCOL_ERROR`, and `QBCL_NO_ERROR` or an unrecognized code on RESET_STREAM or STOP_SENDING reads as `QBCL_TRANSFER_CANCELLED`.
- `Tls`, built from a CA certificate directory and the node's certificate and key, and a QUIC endpoint that accepts and dials sessions with ALPN `qbcl`. Both peers must present a certificate. Each peer's Node IDs are read from the BundleEID otherNames of its certificate (RFC 9174 Section 4.4.2), and a Node ID configured for a dialed peer must be among them or stands in when there are none; a failure terminates the handshake with a bad_certificate alert (Section 4.2). Session resumption is disabled and certificate host names are not checked.
- Every UDP datagram carrying 1-RTT packets is padded to 1200 octets, with path MTU discovery off.
- The unreliable service (Section 4.4): BTP-U messages in QUIC DATAGRAM frames, one link-layer PDU per frame, available only when both peers advertise `max_datagram_frame_size`.
- The reliable service (Section 4.3): each bundle on its own unidirectional stream in the wrapped form, with success reported once the peer has acknowledged every octet and the FIN. A sender or receiver abandoned before the end of its stream cancels it with `QBCL_TRANSFER_CANCELLED`, not quinn's defaults. A framing fault closes the connection with `QBCL_PROTOCOL_ERROR`.
- `Qbcl`, the `hardy-bpa` convergence layer adapter for `ClaAddressType::Quic`, built with `Qbcl::builder()`. Sessions are dialed on demand and each one's peer is reported to the BPA with its certificate Node IDs. Bundles are forwarded and dispatched as streams without being buffered whole, and transfer outcomes are reported through `transfer_outcome`. The receive side cancels oversized bundles with `QBCL_BUNDLE_TOO_LARGE` and BPv6 bundles with `QBCL_UNSUPPORTED_BUNDLE_VERSION`. A bundle the BPA accepts early is read to its end so that the sender sees the transfer complete.
