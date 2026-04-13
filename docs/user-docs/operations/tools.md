# CLI Tools

Hardy includes command-line tools for network diagnostics and bundle
manipulation. These are built from source — see the
[GitHub README](https://github.com/ricktaylor/hardy#getting-started)
for build instructions.

CLI tools are also available via the `ghcr.io/ricktaylor/hardy/hardy-tools` container
image, which includes `bp`, `bundle`, and `cbor`.

All tools support `--help` for detailed usage information:

```bash
bp --help             # List bp subcommands
bp ping --help        # Full bp ping options

bundle --help         # List bundle subcommands
bundle create --help  # Full create options
bundle sign --help    # Full sign options

cbor --help           # List cbor subcommands
cbor inspect --help   # Full inspect options
```

## bp ping

Send Bundle Protocol ping bundles to a destination endpoint and measure
round-trip times. Provides network diagnostics similar to IP `ping`,
including RTT statistics and packet loss percentage.

`bp ping` embeds a minimal BPA and establishes a direct TCPCLv4
connection — no external BPA deployment is required.

### Usage

```
bp ping [OPTIONS] DESTINATION [PEER]
```

| Argument | Description |
|----------|-------------|
| `DESTINATION` | Destination EID of the echo service, e.g. `ipn:2.7` |
| `PEER` | TCPCLv4 peer address as `HOST:PORT`, e.g. `192.168.1.1:4556` |

### Core Options

| Option | Description | Default |
|--------|-------------|---------|
| `-c`, `--count` *N* | Stop after sending *N* pings. | Continuous |
| `-i`, `--interval` *DUR* | Wait between pings. | `1s` |
| `-s`, `--size` *BYTES* | Target total bundle size (padding added). | Minimal |
| `-w`, `--timeout` *DUR* | Session deadline. | No limit |
| `-W`, `--wait` *DUR* | Wait for responses after last ping (with `-c`). | No limit |
| `-q`, `--quiet` | Suppress per-ping output; show only summary. | Off |
| `-v`, `--verbose`[=*LEVEL*] | Enable verbose output (`trace`, `debug`, `info`, `warn`, `error`). | Off |

### DTN Options

| Option | Description | Default |
|--------|-------------|---------|
| `-t`, `--ttl` *N* | Hop count limit (like IP TTL). | No limit |
| `--lifetime` *DUR* | Bundle lifetime (time-based expiry). | Auto-calculated |
| `--no-sign` | Disable BIB-HMAC-SHA256 signing. Use for ION compatibility. | Signing enabled |
| `--no-payload-crc` | Disable CRC on the payload block only. Use for DTNME compatibility. | CRC enabled |
| `-S`, `--source` *EID* | Source endpoint identifier. | Random IPN EID |

### CLA Options

By default `bp ping` uses an embedded TCPCLv4 CLA and connects directly
to the peer address. For testing with external CLAs (e.g. a standalone
`hardy-tcpclv4-server` or a custom CLA binary), the following options
are available:

| Option | Description | Default |
|--------|-------------|---------|
| `--cla` *NAME_OR_PATH* | CLA to use: `tcpclv4` (built-in) or path to an external CLA binary. | `tcpclv4` |
| `--cla-args` *ARGS* | Arguments to pass to the external CLA binary. | *(none)* |
| `--grpc-listen` *ADDR* | gRPC listen address for external CLA registration. | `[::1]:50051` |

When using an external CLA, `bp ping` starts a gRPC server on
`--grpc-listen` and launches the CLA binary as a child process. The CLA
registers with the embedded BPA via gRPC, just as it would with a full
`hardy-bpa-server` deployment.

Example (using an external CLA binary):

```bash
bp ping --cla /usr/local/bin/hardy-tcpclv4-server \
        --cla-args "--address [::]:4556" \
        --grpc-listen "[::1]:50051" \
        ipn:2.7
```

### TLS Options

These options apply to the built-in TCPCLv4 CLA only.

| Option | Description |
|--------|-------------|
| `--tls-insecure` | Accept self-signed certificates (testing only). |
| `--tls-ca` *DIR* | Directory containing CA certificates (PEM). |

### Output

Successful responses:

```
Reply from ipn:2.7: seq=0 rtt=1.234s
```

With status reports enabled on the network, path visibility is shown:

```
Reply from ipn:2.7: seq=0 rtt=1.234s
  path: ipn:3.0 (fwd 234ms, rcv 230ms) -> ipn:4.0 (fwd 456ms, rcv 450ms) -> ipn:2.7 (dlv 567ms)
```

Summary statistics (on completion or Ctrl+C):

```
--- ipn:2.7 ping statistics ---
5 bundles transmitted, 4 received, 20% loss
rtt min/avg/max/stddev = 1.234s/2.567s/4.891s/1.203s
```

### Examples

Ping a node:

```bash
bp ping ipn:2.7 192.168.1.1:4556
```

Send 10 pings at 500ms intervals:

```bash
bp ping -c 10 -i 500ms ipn:2.7 192.168.1.1:4556
```

Test with 1 KB bundles for MTU probing:

```bash
bp ping -s 1024 ipn:2.7 192.168.1.1:4556
```

Quiet mode with 30-second timeout:

```bash
bp ping -q -w 30s ipn:2.7 192.168.1.1:4556
```

### Interoperability

`bp ping` works with any echo service that reflects bundles unchanged:

| Implementation | Compatible | Notes |
|---------------|------------|-------|
| Hardy echo-service | Yes | |
| HDTN echo | Yes | |
| dtn7-rs dtnecho2 | Yes | |
| uD3TN aap_echo | Yes | |
| ION bpecho | Partial | Use `--no-sign` (ION returns a fixed response, breaking payload verification) |

### Exit Status

| Code | Meaning |
|------|---------|
| `0` | At least one response received. |
| `1` | No responses received, or an error occurred. |

## bundle

A comprehensive tool for creating, inspecting, and manipulating BPv7
bundles. Supports BPSec signing and encryption operations.

For full documentation, see the
[bundle tool README](https://github.com/ricktaylor/hardy/blob/main/bpv7/tools/README.md).

### Key Commands

| Command | Description |
|---------|-------------|
| `bundle create` | Create a new bundle with payload. |
| `bundle inspect` | Display bundle contents (markdown, JSON, or pretty-printed). |
| `bundle validate` | Verify bundle correctness. |
| `bundle rewrite` | Clean and canonicalise a bundle. |
| `bundle extract` | Extract payload or block data, with optional decryption. |
| `bundle sign` | Sign blocks with BIB-HMAC-SHA2. |
| `bundle verify` | Verify integrity signatures. |
| `bundle encrypt` | Encrypt blocks with BCB AES-GCM. |
| `bundle add-block` | Add extension blocks (hop-count, bundle-age, etc.). |
| `bundle update-block` | Modify block payload, flags, or CRC. |
| `bundle update-primary` | Modify primary block fields. |
| `bundle remove-block` | Remove extension blocks. |
| `bundle remove-integrity` | Remove BIB protection. |
| `bundle remove-encryption` | Decrypt and remove BCB protection. |

### Example Workflow

Create a bundle, sign it, and inspect:

```bash
echo "Hello DTN" | bundle create -s ipn:1.0 -d ipn:2.0 - \
  | bundle sign -k key.jwk - \
  | bundle inspect -
```

## cbor

A tool for inspecting and converting CBOR (Concise Binary Object
Representation) data, including CBOR Diagnostic Notation (CDN) support.

For full documentation, see the
[cbor tool README](https://github.com/ricktaylor/hardy/blob/main/cbor/tools/README.md).

### Key Commands

| Command | Description |
|---------|-------------|
| `cbor inspect` | Display CBOR data in CDN, JSON, or hex format. |
| `cbor compose` | Convert CDN or JSON text to CBOR binary. |

### Example

Inspect a bundle's CBOR structure with embedded CBOR decoding:

```bash
cbor inspect -e bundle.cbor
```
