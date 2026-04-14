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

The gRPC server enables external components (CLAs, services, routing
agents) to connect to the BPA. It only starts if at least one service
is enabled.

| Key | Valid Values | Default | Description |
|-----|-------------|---------|-------------|
| `address` | IP:port string | `[::1]:50051` | Listen address for gRPC connections. |
| `services` | List of service names | `[]` | Services to enable. Server does not start if empty. |

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

### Time-Variant Routing (TVR)

For scheduled contact windows (satellite passes, maintenance windows,
recurring links), the TVR agent provides cron-based route scheduling.
TVR runs as a separate process and connects to the BPA as a routing
agent via gRPC — ensure `routing` is included in `grpc.services`.

See the
[TVR README](https://github.com/ricktaylor/hardy/blob/main/tvr/README.md)
for configuration and contact plan format.

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
    address: "[::]:4556"
```

See also:

- [**Storage Backends**](storage.md) -- metadata and bundle storage options
- [**Convergence Layers**](convergence-layers.md) -- TCPCLv4 and TLS configuration
