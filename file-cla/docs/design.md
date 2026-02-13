# hardy-file-cla Design

File-based convergence layer for testing, air-gapped transfers, and integration with systems that cannot use network CLAs.

## Design Goals

- **Simplicity.** Provide the simplest possible CLA requiring no network configuration. Files can be created and examined with standard tools.

- **Cross-platform.** Work on Linux, macOS, and Windows using platform-native filesystem notifications.

- **Testability.** Enable easy bundle injection for testing. Drop a file in a directory, observe BPA behaviour.

This CLA is primarily intended for interoperability testing and development, not production deployments. It lacks durability guarantees (no atomic writes, no retry on failure) that would be expected in a production CLA.

## Architecture Overview

The file CLA operates on directories rather than network connections:

```
External System                     Hardy BPA
      │                                 │
      │  writes file to                 │
      └─────────────────► [outbox] ─────┼──► dispatch()
                          (watched)     │
                                        │
      ┌───────────────────────────────  │
      │  reads file from                │
[peer inbox] ◄────────────── forward() ◄┘
```

- **Ingress (outbox)**: External systems write bundle files to a watched directory. The CLA reads each file, dispatches it to the BPA, then deletes the file.

- **Egress (peer inboxes)**: When the BPA forwards a bundle to a peer, the CLA writes the bundle to that peer's configured inbox directory.

The naming follows email client conventions from the external system's perspective:
- **Outbox**: Contains items spooled for sending (external system writes here, Hardy reads)
- **Inbox**: Contains received items (Hardy writes here, external system reads)

## Key Design Decisions

### Directory-Based Peer Addressing

Rather than network addresses, peers are identified by their Node ID mapped to a filesystem path. During registration, the CLA calls `sink.add_peer()` with a `ClaAddress::Private` containing the directory path as bytes.

When the BPA routes a bundle to a peer, it provides the `ClaAddress::Private` back to the CLA, which extracts the path and writes the bundle file there.

### Debounced Filesystem Watching

The CLA uses the `notify` crate with `notify-debouncer-full` for filesystem monitoring. Events are debounced with a 1-second window to handle editors and tools that create files in multiple steps (write temp file, rename).

Only `Create(File)` events trigger bundle dispatch. This avoids reacting to modifications or deletions.

### Bundle ID-Based File Naming

When writing bundles for egress, files are named using the bundle's source EID and creation timestamp:

```
{source}_{timestamp}[_fragment_{offset}]
```

Characters that are problematic for filesystems (`\`, `/`, `:`, space) are replaced with underscores. This naming allows easy identification of bundle files and avoids collisions.

### Immediate Deletion After Dispatch

After successfully dispatching an ingress bundle to the BPA, the file is deleted immediately. There is no "processed" or "sent items" folder. This keeps the watched directory clean and makes it obvious which bundles have not yet been processed.

## Configuration

| Field | Type | Purpose |
|-------|------|---------|
| `outbox` | `Option<PathBuf>` | Directory to watch for incoming bundle files |
| `peers` | `HashMap<NodeId, PathBuf>` | Map of peer Node IDs to their inbox directories |

Example:
```yaml
file_cla:
  outbox: /shared/to_hardy
  peers:
    "ipn:1.100": /shared/from_hardy/node100
    "ipn:1.101": /shared/from_hardy/node101
```

Directories are created if they don't exist. Paths are canonicalised at startup.

## Use Cases

### Testing

Create bundle files with external tools, drop them in the outbox, observe BPA behaviour. Extract bundles from peer inbox directories for inspection.

### Air-Gapped Transfers

Transfer bundles via physical media between disconnected networks:
1. BPA writes bundles to peer inbox directories
2. Files copied to removable media
3. Media transported to destination
4. Files copied to destination's outbox
5. Destination BPA processes bundles

### Legacy Integration

Systems that cannot implement TCPCLv4 can exchange bundles via shared directories. The interop test plan uses this for ION integration via Docker volumes.

## Integration

### With hardy-bpa

Implements `hardy_bpa::cla::Cla`. Registers peers at startup via `sink.add_peer()`. The BPA routes bundles to file-cla peers like any other CLA.

### With hardy-bpa-server

When compiled with the `file-cla` feature, configuration is loaded from the server's config file.

## Dependencies

| Crate | Purpose |
|-------|---------|
| hardy-bpa | CLA trait definition |
| notify | Cross-platform filesystem notifications |
| notify-debouncer-full | Event debouncing |
| flume | Channel between watcher and dispatcher tasks |
| tokio | Async file I/O |

## Future Work

- **Sent Items folder**: Rather than deleting processed files immediately, move them to a "sent items" directory for debugging and audit purposes. This would mirror the email client convention where sent messages are preserved.

## Testing

- Manual testing via file creation and examination
- [Interoperability Test Plan](../../docs/interop_test_plan.md) - ION integration via shared Docker volumes
