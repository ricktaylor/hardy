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
  ipn:2.*.* via ipn:2.1.0  ──────► │  ┌─────────────────────────┐    │
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

See the [User Documentation](https://ricktaylor.github.io/hardy/configuration/bpa-server/#static-routes-file-based-routing) for the complete file format reference, including pattern syntax, actions (`via`, `drop`, `reflect`), priority, and examples.

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

## Future Work

- **RoutingAgent trait**: When the BPA defines a `RoutingAgent` trait, static routes will be refactored into a standalone package implementing that trait. This will enable cleaner separation and potential for multiple routing agents.

- **Include directive**: Support `include /path/to/other/routes` for modular configuration.

- **Metrics export**: Expose route counts and reload events as OpenTelemetry metrics.

## Integration

### With hardy-bpa

Routes are added/removed via `bpa.add_route()` and `bpa.remove_route()`. The BPA's RIB combines these with routes from other sources (CLA peer discovery, future routing protocols).

### With hardy-bpa-server

Currently embedded in bpa-server. Configuration is loaded from the server's config file. The file watcher task runs under the server's task tracker.
