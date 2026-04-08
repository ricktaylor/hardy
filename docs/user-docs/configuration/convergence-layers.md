# Convergence Layers

Convergence Layer Adapters (CLAs) handle the transport of bundles
between DTN nodes over underlying network protocols.

## `clas` — CLA Instances

CLAs are defined as a list in the BPA server configuration. Each entry
defines one CLA instance.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `name` | String | *Required* | Unique name for this CLA instance. Used in logging and metrics. |
| `type` | `tcpclv4`, `file` | *Required* | CLA type to configure. |

Multiple CLA instances can be defined (e.g. separate uplink and
downlink interfaces):

```yaml
clas:
  - name: uplink
    type: tcpclv4
    address: "[::]:4556"

  - name: downlink
    type: tcpclv4
    address: "[::]:4557"
```

## TCPCLv4

The TCP Convergence Layer Protocol Version 4
([RFC 9174](https://datatracker.ietf.org/doc/html/rfc9174)) provides
reliable bundle transfer over TCP connections.

### Connection Options

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `address` | IP:port string | `[::]:4556` | Listen address and port. Use `[::]:4556` for all interfaces or `127.0.0.1:4556` for localhost only. |
| `segment-mru` | Positive integer (bytes) | `16384` | Maximum Receive Unit for a single TCP segment payload. Increase to `65536` for high-bandwidth links. |
| `transfer-mru` | Positive integer (bytes) | `562949953421312` (2^49) | Maximum bundle size that can be received. Set to `1073741824` (1 GB) for large file transfers. |
| `max-idle-connections` | Non-negative integer | `6` | Maximum idle incoming connections per remote IP address. Increase for high-fan-in topologies. |

### Session Parameters

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `contact-timeout` | Positive integer (seconds) | `15` | Time to wait for a CONTACT header from a connecting peer. Increase to `30` for high-latency links. |
| `keepalive-interval` | Non-negative integer (seconds) | `60` | Interval for keepalive signals on idle connections. `0` disables. Use `120` for satellite links. |
| `require-tls` | `true`, `false` | `false` | Require TLS. Reject peers that do not offer TLS. Requires a `tls` block. |

### `tls` — TLS Configuration

When a `tls` block is present on a CLA entry, TLS is offered to peers.
When `require-tls: true`, plaintext connections are rejected.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `cert-file` | File path | *Required* | Server certificate in PEM format. |
| `private-key-file` | File path | *Required* | Private key in PEM format. |
| `ca-certs` | File or directory path | *(optional)* | CA certificates for verifying peers. Can be a single PEM file or a directory of certificates. |
| `server-name` | Hostname string | *(optional)* | Expected server name for SNI verification. |

Example:

```yaml
clas:
  - name: secure-link
    type: tcpclv4
    require-tls: true
    tls:
      cert-file: /etc/hardy/certs/server.crt
      private-key-file: /etc/hardy/private/server.key
      ca-certs: /etc/hardy/ca/trusted.pem
      server-name: ground-station.example.com
```

#### `tls.debug` — Development Options

!!! warning
    These options are insecure and must not be used in production.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `accept-self-signed` | `true`, `false` | `false` | Accept self-signed certificates from peers. |

## File CLA

The file-based CLA transfers bundles via the filesystem — useful for
air-gapped networks, removable media, or integration with external
transfer mechanisms.

Inbound bundles are picked up from an **outbox** directory (watched for
new files). Outbound bundles are written to per-peer **inbox**
directories.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `outbox` | Directory path | *(optional)* | Directory to watch for inbound bundle files. Each file is dispatched to the BPA and then deleted. If omitted, the CLA will not read bundles from the filesystem. |
| `peers` | Map of NodeId to directory path | *(optional)* | Per-peer inbox directories. Bundles forwarded to a peer are written as files in the corresponding directory. If omitted, the CLA will not write bundles to the filesystem. |

Example:

```yaml
clas:
  - name: file-transfer
    type: file-cla
    outbox: /var/spool/hardy/file-cla/outbox
    peers:
      "ipn:2.0": /var/spool/hardy/file-cla/inbox/node2
      "ipn:3.0": /var/spool/hardy/file-cla/inbox/node3
```

Directories are created automatically if they do not exist.

## Standalone CLA Servers

For distributed deployments, CLAs can run as separate processes
connecting to the BPA via gRPC. See the
[distributed deployment](../getting-started/docker.md#distributed-deployment)
guide.

The standalone TCPCLv4 server (`hardy-tcpclv4-server`) uses the
following top-level options in addition to the TCPCLv4-specific options
above. The TCPCLv4 options are flattened to the top level (not nested).

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `bpa-address` | URL string | *Required* | BPA gRPC endpoint to connect to. |
| `cla-name` | String | *Required* | Name to register with the BPA. |
| `log-level` | `trace`, `debug`, `info`, `warn`, `error` | `error` | Logging verbosity. |

The default configuration file is `hardy-tcpclv4.yaml` in the current
directory. Environment variable prefix is `HARDY_TCPCLV4_`.

Example:

```yaml
bpa-address: "http://[::1]:50051"
cla-name: remote-tcpclv4
log-level: info
address: "[::]:4556"
keepalive-interval: 120
require-tls: true
tls:
  cert-file: /etc/hardy/certs/server.crt
  private-key-file: /etc/hardy/private/server.key
  ca-certs: /etc/hardy/ca/trusted.pem
```

See also:

- [**BPA Server**](bpa-server.md) -- core BPA configuration
- [**Docker Deployment**](../getting-started/docker.md) -- distributed container setup
