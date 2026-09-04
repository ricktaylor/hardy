# BPA Server Configuration

Core configuration for `hardy-bpa-server` — the Bundle Processing Agent.

## Configuration File

The BPA server reads configuration from a file in YAML, TOML, or JSON
format (auto-detected from the file extension). The file is located
using the following precedence:

1. `--config <file>` command-line argument
2. `HARDY_BPA_SERVER_CONFIG_FILE` environment variable
3. Default path (see below)

The default configuration file is `hardy-bpa-server.yaml` in a
platform-dependent directory:

| Platform | Default path |
|----------|-------------|
| Linux (with `$HOME`) | `$HOME/.config/hardy-bpa-server/hardy-bpa-server.yaml` (XDG) |
| Linux (no `$HOME`) | `/etc/opt/hardy-bpa-server/hardy-bpa-server.yaml` |
| macOS | `$HOME/Library/Application Support/dtn.Hardy.hardy-bpa-server/hardy-bpa-server.yaml` |
| Windows | `C:\Users\<user>\AppData\Roaming\Hardy\hardy-bpa-server\config\hardy-bpa-server.yaml` |

!!! note
    The default path looks specifically for a `.yaml` file. When using
    `--config` or the environment variable, the file extension determines
    the format — `.yaml`, `.toml`, and `.json` are all supported. All
    examples in this guide use YAML.

The schema is strict: an unknown key anywhere in the file is a startup error naming the known keys, so a key from an earlier release, or a typo, cannot silently leave a default in force. The exceptions are the extension points: `clas` entries of unknown `type` and unknown `policies` types are ignored with a warning, so a config can name extensions this build was not compiled with.

Example:

```bash
hardy-bpa-server --config /etc/hardy/config.yaml
```

## Environment Variable Overrides

Any configuration option can be overridden via environment variables
using the `HARDY_BPA_SERVER_` prefix with underscores replacing hyphens
and dots. For example:

| Config key | Environment variable |
|------------|---------------------|
| `log-level` | `HARDY_BPA_SERVER_LOG_LEVEL` |
| `node-ids` | `HARDY_BPA_SERVER_NODE_IDS` |

## Top-Level Options

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `node-ids` | String or list of EID strings | Random IPN EID | Endpoint IDs that identify this node. Supports `ipn:` and `dtn:` schemes. |
| `log-level` | `trace`, `debug`, `info`, `warn`, `error` | `info` | Logging verbosity. Also settable via `--log-level` CLI argument. |
| `status-reports` | `true`, `false` | `false` | Whether to generate and dispatch bundle status reports. See warning below. |
| `processing-pool-size` | Positive integer | 4 &times; CPU cores | Maximum concurrent bundle processing tasks. |
| `poll-channel-depth` | Positive integer | `16` | Depth of the internal channel used for polling for new bundles. |
| `max-bundle-size` | Positive integer | `67108864` (64 MiB) | Maximum size in bytes of a single reassembled bundle, enforced where streamed ingress and origination accumulate segments. Streams exceeding it are rejected. Raise this when peers transfer large ADUs. The default is sized for the current in-memory accumulation and will be revisited when storage spooling lands; treat it as a custody-admission bound, not a protocol limit. |
| `service-priority` | Non-negative integer | `1` | Routing priority for service registration routes. See [Route Selection Order](#route-selection-order) for how priority interacts with pattern specificity. |

!!! warning
    RFC 9171 §5.1: *"the requesting of status reports for large numbers
    of bundles could result in an unacceptable increase in the bundle
    traffic in the network. For this reason, the generation of status
    reports MUST be disabled by default and enabled only when the risk
    of excessive network traffic is deemed acceptable."*

Example (single EID):

```yaml
node-ids: "ipn:1.0"
```

Example (multiple EIDs across both schemes):

```yaml
node-ids:
  - "ipn:1.0"
  - "dtn://my-node/"
```

## `grpc` — Management Interface

The gRPC server enables external components (CLAs, services,
applications, routing agents) to connect to the BPA. An absent `grpc`
section runs no gRPC server; a present section must enable at least one
service, or parsing fails.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `address` | IP:port string | `[::1]:50051` | Listen address for gRPC connections; the port is claimed at startup, so a conflict is a startup error. |
| `services` | A list drawn from `cla`, `service`, `application`, `routing`. `service` components exchange whole BPv7 bundles; `application` components exchange payloads (ADUs) | - | Required; list at least one, with no repeats, or parsing fails. A typo'd name is a parse error listing the known ones. |
| `drain-timeout` | humantime duration string, e.g. `5s`, `1m 30s`; `0s` cuts open connections immediately | `5s` | How long a graceful shutdown waits for open gRPC connections to drain before abandoning them (a client holding an unread response stream can otherwise stall shutdown indefinitely). |

Example (standalone deployment):

```yaml
grpc:
  address: "[::1]:50051"
  services: ["application", "service"]
```

Example (distributed deployment with external CLAs and routing agents):

```yaml
grpc:
  address: "[::]:50051"
  services: ["application", "cla", "service", "routing"]
```

Available service names:

| Service | Purpose |
|---------|---------|
| `application` | High-level send/receive API for applications |
| `service` | Low-level raw bundle API |
| `cla` | Convergence Layer Adapter registration |
| `routing` | Routing agent registration (for external agents like TVR) |

!!! tip
    For standalone deployments with inline CLAs, you may only need
    `application` and `service`. Enable `cla` and `routing` when running
    separate CLA or routing agent containers.

### `grpc.tls` — Listener TLS

An absent `tls` block serves plaintext HTTP/2; a present block enforces TLS for the whole port (there is no in-band negotiation, so no `required` key exists). Because this is a listener, the dial-side keys of the CLA `tls` vocabulary (`server-name`, `insecure-skip-verify`) do not apply and are not accepted.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `identity` | Object with `cert-file` and `key-file` | - | Required. The server's own certificate and private key, presented to every client. The two fields are only representable as a pair, so a lone certificate or key is a parse error. |
| `identity.cert-file` | File path | - | The server's certificate (PEM). |
| `identity.key-file` | File path | - | The private key (PEM: PKCS#8, PKCS#1, or SEC1) matching `cert-file`. `private-key-file` is accepted as an alias. |
| `client-auth` | `off`, `optional`, `required` | `off` | Client-certificate verification for inbound connections (mutual TLS): `off` never requests a certificate, `optional` verifies one when presented but accepts clients without one, `required` refuses clients without a certificate chaining to `ca-certs`. Any value other than `off` requires `ca-certs`. |
| `ca-certs` | File path | *(none)* | A PEM file of CA certificates (one file, one or more certificates) used to verify client certificates under mutual TLS. Required when `client-auth` is not `off`, ignored otherwise. |

Example:

```yaml
grpc:
  services: ["application", "cla"]
  tls:
    identity:
      cert-file: "/etc/hardy/certs/server.crt"
      key-file: "/etc/hardy/private/server.key"
    client-auth: "required"
    ca-certs: "/etc/hardy/ca/clients.pem"
```

### `grpc.http2` — Transport Tuning

HTTP/2 transport tuning for the gRPC listener. Every key is optional; absent keys defer to the server's own defaults, which favour throughput at scale (a fixed ~64 KiB window would otherwise cap a single transfer at window/round-trip-time). All sizes are in bytes.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `adaptive-window` | `true`, `false` | `true` | Auto-size the stream and connection flow-control windows to the connection's bandwidth-delay product. When on, the fixed `initial-*-window-size` keys below are ignored; set `false` to pin fixed windows instead. Note that with adaptive windows, one stalled consumer can hold the connection-window budget it has grown on its connection (head-of-line blocking); pinned fixed windows bound that. |
| `initial-stream-window-size` | Integer in `1..=2147483647` (2^31 - 1, RFC 9113 §6.9.1) | *(transport default)* | Fixed initial per-stream receive window. Ignored while `adaptive-window` is on. Out-of-range values, including zero (which would wedge every stream), are a parse error. |
| `initial-connection-window-size` | Integer in `1..=2147483647` (2^31 - 1, RFC 9113 §6.9.1) | *(transport default)* | Fixed initial whole-connection receive window. Ignored while `adaptive-window` is on. Out-of-range values, including zero (which would wedge every stream), are a parse error. |
| `max-concurrent-streams` | Positive integer | *(transport default, ~200)* | Maximum concurrent HTTP/2 streams a peer may open. Zero would wedge the listener, so it is a parse error. Bounds per-connection memory (window &times; streams), and doubles as a throughput knob: each transfer is its own RPC, so this caps a connection's concurrent in-flight transfers. Raise it, or pool connections client-side, to push more transfers in parallel. |
| `max-frame-size` | Integer in `16384..=16777215` (2^14 to 2^24 - 1, RFC 9113 §6.5.2) | `1048576` (1 MiB) | Maximum HTTP/2 DATA frame payload, defaulting to one data-plane chunk per frame. Larger frames cut per-frame bookkeeping for big transfers. Out-of-range values are a parse error. |

Example:

```yaml
grpc:
  services: ["application"]
  http2:
    adaptive-window: false
    initial-stream-window-size: 16777216       # 16 MiB
    initial-connection-window-size: 134217728  # 128 MiB
    max-concurrent-streams: 1024
```

## `built-in-services` — Application Services

Built-in services are configured as key-value pairs. Each key is a
service name; the value is a list of service identifiers to register on.
Integers are IPN service numbers, strings are DTN service names. Omit a
key entirely to disable that service.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `echo` | List of integers and/or strings | *(disabled)* | Echo service endpoints. `[7]` registers on IPN service 7 only; `[7, "echo"]` also registers on DTN service `echo`. |

### Echo Service

The echo service reflects incoming bundles back to the sender with the
payload unchanged. It is used for network diagnostics with `bp ping`
and for verifying end-to-end connectivity between nodes.

To enable the echo service, register it on one or more service
endpoints:

```yaml
built-in-services:
  echo: [7]
```

This registers the echo service on IPN service number 7, which is the
conventional endpoint for echo. A bundle sent to `ipn:<node>.7` will
be reflected back to the source. For example, if this node is
`ipn:1.0`, then `bp ping ipn:1.7 <peer>` will reach the echo service.

To also register on a DTN service name:

```yaml
built-in-services:
  echo: [7, "echo"]
```

This allows both `ipn:<node>.7` and `dtn://<node>/echo` to reach the
service.

## `static-routes` — File-Based Routing

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `routes-file` | File path | *(none)* | Path to the static routes file. |
| `watch` | `true`, `false` | `true` | Monitor the file for changes and reload automatically. |
| `priority` | Non-negative integer | `100` | Default priority for routes from this file. Lower values are preferred; `10` takes precedence over `100`. |
| `protocol-id` | String | `static_routes` | Protocol identifier used when registering routes with the RIB. |

### Route File Format

The routes file is line-oriented, one route per line. Lines starting
with `#` are comments. Blank lines are ignored.

```
<eid-pattern> via <next-hop-eid> [priority <n>]
<eid-pattern> drop [<reason-code>]
<eid-pattern> reflect [priority <n>]
```

| Action | Description |
|--------|-------------|
| `via <eid>` | Forward bundles to the next-hop EID for recursive route lookup. |
| `drop [<reason>]` | Discard bundles, optionally with a BPv7 status report reason code. |
| `reflect` | Return bundles to the previous hop. Useful for diagnostics. |

Examples:

```
# Forward all traffic for node 2 via its admin endpoint
ipn:2.*.* via ipn:2.1.0

# Drop traffic to node 9 (maintenance window)
ipn:9.*.* drop

# Forward with explicit priority
ipn:3.*.* via ipn:3.1.0 priority 10

# Reflect all DTN-scheme traffic (diagnostic)
dtn://**/** reflect priority 1200
```

A full example is available at
[`examples/static_routes`](https://github.com/ricktaylor/hardy/blob/main/bpa-server/examples/static_routes).

### Time-Variant Routing (TVR)

For scheduled contact windows (satellite passes, maintenance windows,
recurring links), TVR runs as a separate process and connects to the
BPA as a routing agent via gRPC — ensure `routing` is included in
`grpc.services`.

See the [TVR configuration reference](tvr.md) for configuration,
contact plan format, gRPC service, and hot-reload.

## Route Selection Order

The RIB evaluates routes using a three-level ordering:

1. **Priority** (lower values checked first). Admin endpoints and CLA
   peers are at priority 0, services default to 1 (configurable via
   `service-priority`), static routes default to 100.

2. **Pattern specificity** (most specific first within a priority).
   Exact EIDs score highest, then narrow wildcards, then broad
   catch-alls. For example, `ipn:1.2.3` is checked before `ipn:1.2.*`,
   which is checked before `ipn:*.*.*`.

3. **Action precedence** (within a single pattern). When multiple
   actions exist under the same pattern, precedence is:
   Drop > Local service > Forward > Reflect > Via.

Once a pattern matches and yields any result, no further patterns are
consulted. Multiple `via` or forward entries under the **same pattern**
accumulate as equal-cost next hops for ECMP selection. Entries from
different patterns — even at the same priority — are not combined.

### Unregistered Services and Default-to-Wait

When no route matches, the bundle waits for a future route to appear.
This applies to both forwarding destinations and local services — a
bundle addressed to an unregistered service number will wait until that
service registers.

Operators who want to reject bundles for specific service ranges
(rather than waiting indefinitely) must configure an explicit `drop`
rule at a priority that will be checked before the service route. For
example, to drop all unregistered services on this node:

```
ipn:!.* drop priority 2
```

The `!` matches the local node's IPN node number. This works because
the `drop` rule at priority 2 is checked after registered services
(priority 1 by default), but matches any service EID that no
registered service has claimed.

## `rfc9171-validity` — RFC 9171 Validity Filters

These control SHOULD-level requirements from RFC 9171 that can be
relaxed for interoperability with other implementations.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `primary-block-integrity` | `true`, `false` | `true` | Require the primary block to be protected by CRC or BIB. Set `false` for interop with dtn7-rs and other implementations that omit CRCs. |
| `bundle-age-required` | `true`, `false` | `true` | Require a Bundle Age block when creation timestamp is zero. Set `false` for peers without a clock that omit Bundle Age. |

## `ipn-legacy-nodes` — IPN Legacy Filter

Rewrites 3-element IPN EIDs (RFC 9758) to legacy 2-element format for
peers that require the older encoding.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `ipn-legacy-nodes` | List of EID pattern strings | `[]` | Patterns matching nodes that require legacy 2-element IPN encoding. |

Example showing the supported pattern types:

```yaml
ipn-legacy-nodes:
  - "ipn:10.*"           # All endpoints on node 10
  - "ipn:20.0"           # Node 20's admin endpoint
  - "ipn:[100-199].*"    # Nodes 100-199 (range pattern)
```

## Complete Example

A production-ready configuration:

```yaml
log-level: info
node-ids:
  - "ipn:42.0"
  - "dtn://ground-station-1/"

grpc:
  address: "[::]:50051"
  services: ["application", "cla", "service", "routing"]

storage:
  lru-capacity: 4096
  max-cached-bundle-size: 65536
  metadata:
    type: postgres
    database-url: "postgresql://hardy:secret@db.internal/hardy"
  bundle:
    type: s3
    bucket: hardy-bundles
    region: eu-west-1

built-in-services:
  echo: [7]

static-routes:
  routes-file: "/etc/hardy/routes"
  watch: true
  priority: 100

clas:
  - name: uplink
    type: tcpclv4
    listeners: ["[::]:4556"]
```

See also:

- [**Storage Backends**](storage.md) -- metadata and bundle storage options
- [**Convergence Layers**](convergence-layers.md) -- TCPCLv4 and TLS configuration
