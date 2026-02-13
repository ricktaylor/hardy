# hardy-tools Design

Command-line diagnostic and testing tools.

## Design Goals

- **ION interoperability.** Use payload formats compatible with ION's `bping`/`bpecho` utilities. Enables testing connectivity between Hardy and ION deployments.

- **Self-contained operation.** Operate independently without requiring a full BPA deployment. The tools embed minimal BPA functionality with no persistent storage.

- **Network diagnostics.** Measure round-trip times and verify end-to-end connectivity for troubleshooting.

## Architecture Overview

The `bp` binary embeds a minimal BPA and TCPCLv4 CLA:

```
┌─────────────────────────────────────────────────────────┐
│                    bp ping                              │
│  ┌────────────────────────────────────────────────┐    │
│  │  hardy-bpa (no storage)                         │    │
│  │  ┌─────────────┐  ┌──────────────────────────┐ │    │
│  │  │ PingService │  │ hardy-tcpclv4            │ │    │
│  │  │ (Application│  │ (direct TCP connection)  │ │    │
│  │  │  trait)     │  │                          │ │    │
│  │  └─────────────┘  └──────────────────────────┘ │    │
│  └────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

This architecture enables testing without deploying a full BPA server. The BPA has no storage backend - bundles exist only transiently during the ping operation.

## Key Design Decisions

### Embedded BPA Rather Than Client

The tool embeds a full BPA rather than acting as a client to an external BPA. This avoids deployment complexity - users can test connectivity without configuring a server. The BPA handles all bundle processing, routing, and CLA integration internally.

### ION Text Format for Payloads

Ping payloads use ION's text format: `<flag> <seqno> <sec> <nsec>` as space-separated integers. This enables interoperability with ION's `bpecho` service. A binary format (compatible with microsDTN and DTN2) is available but currently disabled.

### Application Trait for Ping Service

The ping service implements `Application` rather than `Service`. This provides the high-level API with send/receive operations and automatic bundle construction, appropriate for a user-facing tool.

## Usage

```
bp ping <destination> [peer_address]
```

**Arguments:**
- `destination` - Destination EID to ping
- `peer_address` - Optional TCPCLv4 address (e.g., `192.168.1.1:4556`)

**Options:**
- `-c, --count` - Number of bundles to send
- `-i, --interval` - Time between bundles (default: 1s)
- `-l, --lifetime` - Bundle lifetime (auto-calculated if not specified)
- `-w, --wait` - Time to wait for responses after sending
- `-r, --flags` - Status reporting flags (rcv, fwd, dlv, del)
- `-s, --source` - Source EID (random if not specified)
- `--tls-accept-self-signed` - Accept self-signed TLS certificates
- `--tls-ca-bundle` - CA bundle directory for TLS validation

## ION Interoperability

The ping payload format matches ION's `bping`:

```
<service_flag> <seqno> <unix_sec> <nsec>
```

Where `service_flag` is 0x01 for ping requests and 0x02 for responses. This enables testing against ION nodes running `bpecho`.

## Integration

### With hardy-bpa

The tool instantiates a `Bpa` with minimal configuration and no storage. Routes are added dynamically for the ping destination.

### With hardy-tcpclv4

TCPCLv4 is embedded for direct peer connection. The tool establishes a single connection to the specified peer address, sends ping bundles, and receives responses.
