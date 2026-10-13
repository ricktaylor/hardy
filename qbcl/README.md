# hardy-qbcl

A Qubicle (DTN QUIC) Bundle Protocol convergence layer.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

`hardy-qbcl` implements the convergence layer of [draft-ek-dtn-qubicle](https://datatracker.ietf.org/doc/draft-ek-dtn-qubicle/), which carries bundles over QUIC (RFC 9000): one bundle per unidirectional stream for reliable transfer, and BTP-U messages in QUIC DATAGRAM frames (RFC 9221) for the optional unreliable service. `Qbcl` is the `hardy-bpa` convergence layer adapter. It carries every bundle on the reliable service. The unreliable service is implemented but the adapter does not yet use it.

## Features

- Incremental, zero-copy reader for stream content (Section 4.3.2) that checks the direct and wrapped forms, reports the bundle version and declared length from the first octets, and detects excess or truncated data at FIN
- Shortest-form encoder for the wrapped form's byte string head
- The Section 5 application error codes, with the rules for reading unrecognized or misplaced codes on CONNECTION_CLOSE and on RESET_STREAM/STOP_SENDING
- Mutual TLS 1.3 sessions over [quinn](https://crates.io/crates/quinn), with each peer identified by the Node IDs in its certificate's BundleEID otherNames (RFC 9174 Section 4.4) and an optional configured Node ID that must match or stand in for an absent one (Section 4.2)
- Every 1-RTT UDP datagram padded to 1200 octets, so that bundle sizes are not visible to an observer
- The unreliable service over QUIC DATAGRAM frames (Section 4.4), with BTP-U segmenting bundles larger than one datagram
- The reliable service (Section 4.3): one bundle per unidirectional stream in the wrapped form, success when the peer has acknowledged every octet and the FIN, and cancellation with the Section 5 codes
- A `hardy-bpa` convergence layer adapter that dials sessions on demand, reports peers by their certificate Node IDs, streams bundles in both directions without buffering them whole, and refuses oversized and BPv6 bundles from the stream head

## Documentation

- [Design](docs/design.md)
- [API Documentation](https://docs.rs/hardy-qbcl)

## Licence

Apache 2.0 -- see [LICENSE](../LICENSE)
