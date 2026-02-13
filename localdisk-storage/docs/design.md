# hardy-localdisk-storage Design

Local filesystem bundle storage implementing the BundleStorage trait.

## Design Goals

- **Crash safety.** Bundle data must not be corrupted by power failure or process crash. Partial writes should never leave invalid state in storage.

- **Filesystem-friendly.** Avoid creating too many files in a single directory, which degrades performance on many filesystems. Distribute files across subdirectories.

- **Zero-copy loading.** Support memory-mapped file access where possible, allowing the kernel to handle caching and avoiding explicit read operations.

- **Parallel recovery.** On restart, scan the storage directory quickly using multiple threads to minimise startup time.

## Directory Structure

Bundles are stored in a two-level directory hierarchy using random hexadecimal paths:

```
store_dir/
  ab/
    cd/
      12345678
      9abcdef0
    ef/
      ...
  ...
```

The two-level structure (256 × 256 = 65536 possible directories) prevents any single directory from accumulating too many entries. Many filesystems slow down significantly when directories contain thousands of files; this distribution keeps individual directories small.

File names are random 32-bit hex values. On collision, the value is incremented until a unique name is found. The randomness avoids predictable patterns that might interact poorly with filesystem caching.

## Atomic Write Protocol

Writes follow a careful protocol to ensure crash safety:

1. Generate a random file path within the two-level structure
2. Write bundle data to a temporary file (same directory, `.tmp` extension)
3. `fsync` the temporary file (ensures data is on disk)
4. Atomic rename from temporary to final path
5. `fsync` the parent directory (ensures the rename is durable)

This protocol means bundles are either fully present or fully absent - never partially written. A crash during step 2 leaves a `.tmp` file that recovery will clean up. A crash after step 4 leaves a complete bundle.

The `fsync` operations have performance cost. Configuration allows disabling them (`fsync: false`) for deployments where the underlying storage provides its own durability guarantees (e.g., battery-backed controllers).

## Memory-Mapped Loading

When the `mmap` feature is enabled, bundle loading uses memory-mapped files instead of explicit reads. The kernel maps the file into the process address space; actual disk reads happen on demand as pages are accessed.

This provides several benefits. The kernel manages caching automatically. Multiple readers of the same bundle share physical memory. Large bundles don't require contiguous heap allocation.

## Recovery Process

On startup, the storage performs a parallel directory walk:

1. Walk starts from the storage root
2. Multiple threads process subdirectories concurrently
3. Each bundle file emits `(storage_name, creation_timestamp)` to the recovery channel
4. Temporary files (`.tmp` extension) are deleted
5. Zero-length placeholder files are deleted
6. Empty directories are removed after processing
7. Files created after the walk started are ignored (they're new bundles, not recovery candidates)

The parallel walk scales with available CPUs, reducing startup time for large storage directories.

## Platform Defaults

Default storage locations follow platform conventions:

| Platform | Default Path |
|----------|--------------|
| Linux | `~/.cache/localdisk-storage` |
| macOS | `~/Library/Caches/com.Hardy.localdisk-storage` |
| Windows | `%LOCALAPPDATA%\Hardy\localdisk-storage\cache` |

Fallback paths are used when user directories aren't available:
- Unix: `/var/spool/localdisk-storage`
- Windows: `localdisk-storage` in executable directory

## Integration

### With hardy-bpa

This library implements the `BundleStorage` trait defined in hardy-bpa. The BPA calls `save()`, `load()`, `delete()`, and `recover()` without knowing the underlying storage mechanism.

### With hardy-bpa-server

The server instantiates localdisk-storage based on configuration and injects it into the BPA. Configuration options (store directory, fsync setting) come from the server's config file.

## Configuration

| Option | Default | Purpose |
|--------|---------|---------|
| `store_dir` | Platform-specific (see above) | Root directory for bundle storage |
| `fsync` | `true` | Enable atomic write protocol with fsync for crash safety |

## Dependencies

Feature flags control optional functionality:

- **`mmap`** (default): Memory-mapped file loading via memmap2. Provides zero-copy access but requires local filesystem (not NFS/SMB).
- **`serde`**: Configuration serialization support.
- **`tracing`**: Span instrumentation for async operations.

Key external dependencies:

| Crate | Purpose |
|-------|---------|
| hardy-bpa | `BundleStorage` trait definition |
| memmap2 | Memory-mapped file I/O (optional) |
| directories | Platform-specific default paths |
| tokio | Async filesystem operations |

## Future Work

### Capacity Limits and Eviction (REQ-7.1.2, REQ-7.1.3)

The requirements specify configurable maximum storage capacity and bundle eviction policies when capacity is reached. This is not yet implemented. Future work includes:

- Configurable maximum total bytes for bundle storage
- Eviction policy integration with BPA's `storage_priority` metadata
- Disk usage monitoring and alerting

## Testing

- [Test Plan](test_plan.md) - Filesystem persistence verification
