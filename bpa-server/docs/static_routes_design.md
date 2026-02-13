# Static Routes Design

File-based static routing for DTN bundle forwarding, inspired by IP routing table configuration.

## Design Goals

- **Familiar syntax.** Provide a routing file format that feels natural to network engineers familiar with IP routing configuration (`/etc/network/routes`, `ip route`, routing table entries).

- **Hot-reload.** Support live reconfiguration without restarting the BPA. Operators can update routing topology by editing a file, and changes take effect immediately.

- **Integration with RIB.** Feed routes into the BPA's Routing Information Base alongside dynamic routes from CLAs and future routing protocols.

## Architecture Overview

Static routes bridge a configuration file to the BPA's routing system:

```
                                   ┌─────────────────────────────────┐
  /etc/.../static_routes           │              BPA                │
  ─────────────────────            │                                 │
  ipn:2.*.* via ipn:0.1.0  ──────► │  ┌─────────────────────────┐    │
  ipn:3.*.* drop           ──────► │  │  Routing Information    │    │
  dtn://**/** reflect      ──────► │  │  Base (RIB)             │    │
                                   │  │                         │    │
  ┌──────────────────┐             │  │  ┌─────────────────┐    │    │
  │  File Watcher    │─────────────┼─►│  │ static_routes   │    │    │
  │  (notify)        │  add/remove │  │  │ protocol_id     │    │    │
  └──────────────────┘             │  │  └─────────────────┘    │    │
                                   │  │  ┌─────────────────┐    │    │
                                   │  │  │ CLA-discovered  │    │    │
                                   │  │  │ peers           │    │    │
                                   │  │  └─────────────────┘    │    │
                                   │  └─────────────────────────┘    │
                                   └─────────────────────────────────┘
```

Routes are identified by their `protocol_id` (default: `"static_routes"`), allowing the RIB to distinguish static routes from other sources.

## File Format

The routes file uses a line-oriented format inspired by IP routing tables:

```
<pattern> <action> [priority <n>]
```

### Pattern

An EID pattern using hardy-eid-patterns syntax:

| Pattern | Matches |
|---------|---------|
| `ipn:2.*.*` | All services on any node in allocator 2 |
| `ipn:*.100.*` | All services on node 100 in any allocator |
| `ipn:1.100.7` | Exactly ipn:1.100.7 |
| `dtn://node/**` | All endpoints under dtn://node/ |
| `dtn://**` | All DTN scheme endpoints |

### Action

One of three routing actions:

**`via <eid>`** - Forward bundles matching the pattern via the specified next-hop EID. The RIB performs recursive lookup to resolve the next-hop to a CLA peer.

```
ipn:2.*.* via ipn:0.1.0
```

This is analogous to `ip route add 10.0.0.0/8 via 192.168.1.1` in IP routing.

**`drop [<reason_code>]`** - Discard bundles matching the pattern. Optionally specify a BPv7 status report reason code (numeric).

```
ipn:99.*.* drop
ipn:98.*.* drop 3
```

This is analogous to a blackhole route (`ip route add blackhole 10.0.0.0/8`).

**`reflect`** - Return bundles to the node that sent them (previous hop), as identified by the Previous Node Block. This implements the optional behavior from RFC 9171 Section 5.4.2 Step 1 ("Forwarding Failed"), which allows the BPA to forward bundles back to the sending node when forwarding is not possible.

```
dtn://test/** reflect
```

### Priority

Routes can specify a priority (lower values = higher priority). If omitted, uses the default priority from configuration.

```
ipn:2.*.* via ipn:0.1.0 priority 50
ipn:2.100.* via ipn:0.2.0 priority 10  # More specific, higher priority
```

### Comments and Whitespace

Lines starting with `#` are comments. Blank lines and leading/trailing whitespace are ignored.

```
# Production routes
ipn:2.*.* via ipn:0.1.0

# Testing - drop all traffic to allocator 99
ipn:99.*.* drop
```

## Key Design Decisions

### IP Routing Analogy

The file format deliberately mirrors IP routing syntax. Network engineers intuitively understand:

- Pattern matching (like CIDR prefixes)
- Next-hop specification (`via`)
- Blackhole routes (`drop`)
- Priority/metric for route selection

This reduces the learning curve for operators already familiar with IP networking.

### Incremental Updates on Hot-Reload

When the routes file changes, the implementation calculates the diff:

1. Routes present before but missing/changed now → remove from RIB
2. Routes present now but missing/changed before → add to RIB

This minimises RIB churn and avoids removing routes that haven't changed.

### Debounced File Watching

File changes are debounced with a 1-second window to handle:

- Editors that write to temp files then rename
- Multiple rapid saves
- Incomplete writes during file copy

Only after the debounce period does the reload trigger.

### Protocol ID Isolation

All static routes are registered under a single `protocol_id`. This allows:

- The RIB to track route ownership
- Clean removal of all static routes if the protocol is disabled
- Future support for multiple static route files with different IDs

### Graceful Error Handling

Parse errors in the routes file don't crash the server. When `watch` is enabled:

- Missing file at startup: warn and continue (file may be created later)
- Parse error during reload: log error, keep existing routes
- File deleted: remove all routes from this protocol

## Configuration

```yaml
static_routes:
  routes_file: /etc/hardy-bpa-server/static_routes
  priority: 100
  watch: true
  protocol_id: static_routes
```

| Option | Default | Description |
|--------|---------|-------------|
| `routes_file` | `<config_dir>/static_routes` | Path to the routes file |
| `priority` | `100` | Default priority for routes without explicit priority |
| `watch` | `true` | Monitor file for changes and hot-reload |
| `protocol_id` | `"static_routes"` | Identifier for these routes in the RIB |

## Examples

### Simple Gateway Configuration

```
# All traffic to allocator 2 goes via gateway node
ipn:2.*.* via ipn:0.1.0
```

### Multi-Path with Priorities

```
# Primary path
ipn:2.*.* via ipn:0.1.0 priority 10

# Backup path (higher priority number = lower preference)
ipn:2.*.* via ipn:0.2.0 priority 100
```

### Blackhole Unwanted Traffic

```
# Block traffic to decommissioned nodes
ipn:1.999.* drop
ipn:1.998.* drop
```

### Testing Configuration

```
# Reflect all test traffic back to source
dtn://test/** reflect

# Normal production routes
ipn:*.*.* via ipn:0.1.0
```

## Future Work

- **RoutingAgent trait**: When the BPA defines a `RoutingAgent` trait, static routes will be refactored into a standalone package implementing that trait. This will enable cleaner separation and potential for multiple routing agents.

- **Include directive**: Support `include /path/to/other/routes` for modular configuration.

- **Metrics export**: Expose route counts and reload events as OpenTelemetry metrics.

## Integration

### With hardy-bpa

Routes are added/removed via `bpa.add_route()` and `bpa.remove_route()`. The BPA's RIB combines these with routes from other sources (CLA peer discovery, future routing protocols).

### With hardy-bpa-server

Currently embedded in bpa-server. Configuration is loaded from the server's config file. The file watcher task runs under the server's task tracker.
