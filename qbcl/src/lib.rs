/*!
Qubicle: the DTN QUIC Bundle Protocol convergence layer.

This crate implements the convergence layer defined in [draft-ek-dtn-qubicle], which carries BPv7 bundles over QUIC (RFC 9000). Reliable transfers put each bundle on its own unidirectional QUIC stream, directly or wrapped in a single CBOR byte string; the optional unreliable service carries BTP-U messages in QUIC DATAGRAM frames (RFC 9221).

[draft-ek-dtn-qubicle]: https://datatracker.ietf.org/doc/draft-ek-dtn-qubicle/

Section references throughout this crate's documentation and comments follow draft-ek-dtn-qubicle-02; this is the only place in the code the target revision is pinned.

The crate holds the stream content framing of Section 4.3.2, the application error codes of Section 5, the QUIC endpoint (mutually authenticated TLS 1.3 sessions whose peers are identified by the Node IDs their certificates carry, Section 4.2), the reliable service of Section 4.3 over unidirectional streams, and the unreliable service of Section 4.4 over QUIC DATAGRAM frames. [`cla::Qbcl`] is the convergence layer adapter that registers with a Hardy BPA; it carries every bundle on the reliable service, and does not yet offer the unreliable one.
*/
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the unreliable service, session close, and a few framing and error-code accessors are used only by tests until the adapter offers the unreliable service"
    )
)]

pub mod cla;
mod datagram;
mod endpoint;
mod error_code;
mod node_id;
mod reliable;
mod stream;
pub mod tls;

/// The TLS Application-Layer Protocol Negotiation identifier of Qubicle (Section 4.1).
pub const ALPN: &[u8] = b"qbcl";
