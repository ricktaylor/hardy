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
    listeners: ["[::]:4556"]

  - name: downlink
    type: tcpclv4
    listeners: ["[::]:4557"]
```

## TCPCLv4

The TCP Convergence Layer Protocol Version 4
([RFC 9174](https://datatracker.ietf.org/doc/html/rfc9174)) provides
reliable bundle transfer over TCP connections.

The schema is strict: an unknown key in a TCPCLv4 section (including its `tls` block) is a startup error naming the known keys, so the removed `address` key of earlier releases, or a typo, cannot silently leave a default in force. Use `listeners` in place of `address`.

### Connection Options

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `listeners` | List of IP:port strings | `["[::]:4556"]` | The listening addresses, one entry per listener. Use `["[::]:4556"]` for all interfaces or `["127.0.0.1:4556"]` for localhost only; absent defaults to the IANA-registered `[::]:4556`, and an empty list (`[]`) disables listening for a dial-only node. |
| `segment-mru` | Positive integer (bytes) | `16384` | Maximum Receive Unit for a single TCP segment payload. Increase to `65536` for high-bandwidth links. Zero is a startup error. |
| `transfer-mru` | Positive integer (bytes) | `1073741824` (1 GiB) | Maximum bundle size that can be received. Zero is a startup error. |
| `max-idle-connections` | Non-negative integer | `6` | Maximum idle incoming connections per remote IP address. Increase for high-fan-in topologies; `0` disables pooling. |
| `max-outstanding-transfers` | Positive integer | `16` | Maximum transfers accepted but not yet resolved with an outcome, per peer. Bounds the bundles held in memory by in-flight and queued transfers to each peer; when reached, further forwards to that peer are held unanswered, which is the flow control back to the BPA. Zero is a configuration error. |
| `connection-rate-limit` | Positive integer (per second) | `64` | Maximum incoming connections accepted per second; the listener delays accepts beyond this rate. Zero is a startup error. |

### Session Parameters

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `contact-timeout` | `1` to `60` (seconds) | `15` | Time to wait for a CONTACT header from a connecting peer. Increase to `30` for high-latency links. RFC 9174 caps the recommendation at 60 seconds; values outside the range are a startup error. |
| `keepalive-interval` | Non-negative integer (seconds) | `60` | Interval for keepalive signals on idle connections. `0` disables; `null` is not accepted. Use `120` for satellite links. |

### `tls` — TLS Configuration

When a `tls` block is present on a CLA entry, TLS is offered to peers; a
trust anchor must be configured inside the block. With `required: true`,
sessions that do not negotiate TLS are refused.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `required` | `true`, `false` | `false` | Refuse sessions that do not negotiate TLS (RFC 9174 "Contact Failure"). |
| `ca-certs` | Directory path | *(see below)* | Directory of PEM CA certificates used to verify peers' certificates: the standing trust anchor for normal operation. |
| `insecure-skip-verify` | `true`, `false` | `false` | Accept any peer certificate without validation. **Testing only; must not be used in production.** Overrides `ca-certs` when both are set: a startup warning names both keys, and the ignored certificates are never loaded, so a debug session is one line to flip. |
| `identity.cert-file` | File path | *(optional)* | The node's certificate, in PEM format. Presented to dialers when accepting TLS connections, and to dialed peers as the client certificate when one is requested (mutual TLS). |
| `identity.key-file` | File path | *(optional)* | Private key matching `identity.cert-file`, in PEM format (PKCS#8, PKCS#1, or SEC1). The pre-rename key `private-key-file` is still accepted as an alias. |
| `client-auth` | `off`, `optional`, `required` | `off` | Mutual TLS for incoming connections. `required` refuses dialers without a certificate chaining to `ca-certs`; `optional` verifies a certificate when one is presented but accepts dialers without one (useful while migrating a fleet); `off` never requests one. Requires `identity` and `ca-certs`. |
| `server-name` | Hostname string | *(optional)* | Expected server name for SNI verification, presented when dialing. |

A trust anchor is mandatory: `ca-certs`, or `insecure-skip-verify` for
testing. The `identity` object holds the certificate and key as a pair
(a lone half is a configuration error); listeners offer TLS only when an
identity is configured, and dialing needs no identity unless the peer
enforces mutual TLS.

Example:

```yaml
clas:
  - name: secure-link
    type: tcpclv4
    tls:
      required: true
      ca-certs: /etc/hardy/ca
      identity:
        cert-file: /etc/hardy/certs/server.crt
        key-file: /etc/hardy/private/server.key
      server-name: ground-station.example.com
```

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
[production deployment](../getting-started/docker.md#production-deployment)
guide.

The standalone TCPCLv4 server (`hardy-tcpclv4-server`) uses the
following top-level options in addition to the TCPCLv4-specific options
above. The TCPCLv4 options are flattened to the top level (not nested).

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `bpa-address` | URL string | *Required* | BPA gRPC endpoint to connect to. |
| `cla-name` | String | *Required* | Name to register with the BPA. |
| `log-level` | `trace`, `debug`, `info`, `warn`, `error` | `info` | Logging verbosity. |

The default configuration file is `hardy-tcpclv4.yaml` in the current
directory. Environment variable prefix is `HARDY_TCPCLV4_`.

Example:

```yaml
bpa-address: "http://[::1]:50051"
cla-name: remote-tcpclv4
log-level: info
listeners: ["[::]:4556"]
keepalive-interval: 120
tls:
  required: true
  ca-certs: /etc/hardy/ca
  identity:
    cert-file: /etc/hardy/certs/server.crt
    key-file: /etc/hardy/private/server.key
```

See also:

- [**BPA Server**](bpa-server.md) -- core BPA configuration
- [**Docker Deployment**](../getting-started/docker.md) -- distributed container setup
