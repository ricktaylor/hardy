# MTCP/STCP CLA Design

## Overview

The MTCP/STCP CLA (`mtcp-cla`) is a standalone binary for interop testing with
ION and D3TN. It implements two simple TCP framing protocols — MTCP (CBOR byte
string) and STCP (4-byte u32 length prefix) — and registers with a Hardy BPA
via gRPC.

**These are not standards-track protocols.** The implementation lives in
`tests/interop/` and is not a top-level workspace crate.

## Wire Formats

### MTCP (used by ud3tn)

CBOR byte string framing per `draft-ietf-dtn-mtcpcl-01`. Each bundle is
encoded as a CBOR byte string (major type 2):

```
+---------------------------+--------------------+
| CBOR byte string header   | Bundle Data (var.) |
| (1-9 bytes, major type 2) |                    |
+---------------------------+--------------------+
```

| Bundle Size         | Header Bytes | Format                       |
|---------------------|--------------|------------------------------|
| 0-23                | 1            | `0x40 \| length`             |
| 24-255              | 2            | `0x58, length`               |
| 256-65535           | 3            | `0x59, length (2B BE)`       |
| 65536-4294967295    | 5            | `0x5a, length (4B BE)`       |
| > 4294967295        | 9            | `0x5b, length (8B BE)`       |

### STCP (used by ION)

ION's STCP uses a 4-byte big-endian u32 length prefix (`htonl(bundleLength)`),
NOT the CBOR array format from the STCP spec (`draft-burleigh-dtn-stcp-00`).
Zero-length preambles are keepalives (skipped on decode).

```
+-------------------+--------------------+
| Length (4B u32 BE)| Bundle Data (var.) |
+-------------------+--------------------+
```

### Summary

| Protocol | Wire Format | Used By |
|----------|-------------|---------|
| **STCP** (ION impl) | 4-byte big-endian u32 + raw bundle | ION |
| **MTCP** | CBOR byte string (length in header) | D3TN |

## Shared Properties

- **No contact header** (unlike TCPCLv4's `dtn!` magic + version)
- **No session negotiation** (no SESS_INIT, no keepalive, no MRU exchange)
- **No transfer segmentation** (entire bundle sent as one frame)
- **No acknowledgement** (no XFER_ACK)
- **No TLS**
- **Unidirectional sessions** — sender connects, sends bundle(s), receiver reads
- Bidirectional exchange requires two separate TCP connections

## Architecture

```
tests/interop/
├── mtcp/                      ← this crate
│   ├── design.md              ← this file
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs            ← binary entry point, gRPC registration
│       ├── cla.rs             ← Cla trait impl (register, forward)
│       ├── config.rs          ← TOML config (bpa-address, framing, peer, etc.)
│       ├── codec.rs           ← MtcpCodec + StcpCodec (tokio-util Decoder/Encoder)
│       ├── listen.rs          ← TCP listener + per-connection dispatch loop
│       └── connect.rs         ← Connect-per-bundle outbound forwarding
├── ION/                       ← ION interop tests (STCP)
└── ud3tn/                     ← µD3TN interop tests (MTCP, future)
```

The CLA runs as a standalone process and connects to a BPA via gRPC using
`hardy-proto`'s `RemoteBpa` client. It can be used with:

- **`bpa-server`** — configure `[grpc] services = ["cla"]`, start `mtcp-cla`
  as a separate process pointing at the BPA's gRPC address.
- **`bp ping`** — use `--cla /path/to/mtcp-cla --cla-args "--config cla.toml"`
  and bp ping will start a gRPC server and spawn the CLA subprocess.

### Config Example

```toml
bpa-address = "http://[::1]:50051"
cla-name = "cl0"
framing = "stcp"
address = "[::]:4557"
peer = "127.0.0.1:4557"
peer-node = "ipn:2.0"
```

## Connection Lifecycle

**ION STCP** (`stcpclo` sender): maintains long-lived connections with
keepalives (zero-length preambles every 15s). Reconnects with exponential
backoff. `stcpcli` (receiver) accepts connections and loops reading bundles,
skipping zero-length keepalives.

**D3TN MTCP**: connections closed after contact by default
(`CLA_MTCP_CLOSE_AFTER_CONTACT=1`). Python tools connect, send one bundle,
disconnect.

**This CLA** uses connect-per-bundle: open connection, send bundle, close.
Simplest approach, compatible with both ION and D3TN receivers. The STCP
codec skips zero-length preambles on receive to handle ION keepalives.

## Specifications

- **MTCP**: `draft-ietf-dtn-mtcpcl-01` — CBOR byte string framing
- **STCP**: `draft-burleigh-dtn-stcp-00` — CBOR array framing (spec only;
  ION uses 4-byte u32 instead)
