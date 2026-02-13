# hardy-localdisk-storage Design

Local filesystem bundle storage implementing the BundleStorage trait.

## Design Goals

- **Crash safety.** Bundle data must not be corrupted by power failure or process crash. Partial writes should never leave invalid state in storage.

- **Filesystem-friendly.** Avoid creating too many files in a single directory, which degrades performance on many filesystems. Distribute files across subdirectories.

- **Zero-copy loading.** Support memory-mapped file access where possible, allowing the kernel to handle caching and avoiding explicit read operations.

- **Parallel recovery.** On restart, scan the storage directory quickly using multiple threads to minimise startup time.

## Architecture Overview

The storage maps bundle identifiers to files on the local filesystem:

```
BPA
 │
 ├─ save(bundle_data)  ──► write .tmp file ──► fsync ──► atomic rename
 ├─ load(storage_name) ──► mmap file (or read)
 ├─ delete(storage_name) ──► unlink file
 └─ recover() ──► parallel directory walk ──► emit (name, timestamp) pairs
```

Bundles are stored in a two-level directory hierarchy (256 x 256 directories) with random filenames. This distribution prevents any single directory from accumulating too many entries.

## Key Design Decisions

### Two-Level Directory Hierarchy

Many filesystems slow down significantly when directories contain thousands of files. The two-level structure (`ab/cd/12345678`) provides 65,536 possible parent directories, keeping individual directories small even with millions of bundles.

File names are random 32-bit hex values. On collision, the value is incremented until a unique name is found. The randomness avoids predictable patterns that might interact poorly with filesystem caching or create hot directories.

### Atomic Write Protocol

Writes follow a careful protocol to ensure crash safety:

1. Write bundle data to a temporary file (`.tmp` extension) in the target directory
2. `fsync` the temporary file to ensure data reaches disk
3. Atomic rename from temporary to final path
4. `fsync` the parent directory to ensure the rename is durable

This protocol means bundles are either fully present or fully absent. A crash during step 1 leaves a `.tmp` file that recovery will clean up. A crash after step 3 leaves a complete bundle.

The `fsync` operations have performance cost. Configuration allows disabling them for deployments where the underlying storage provides its own durability guarantees (e.g., battery-backed RAID controllers, ZFS with synchronous writes).

### Memory-Mapped Loading

When the `mmap` feature is enabled, bundle loading uses memory-mapped files instead of explicit reads. The kernel maps the file into the process address space; actual disk reads happen on demand as pages are accessed.

This provides several benefits: the kernel manages caching automatically, multiple readers of the same bundle share physical memory, and large bundles don't require contiguous heap allocation.

The trade-off is that memory-mapped files don't work reliably on network filesystems (NFS, SMB). For such deployments, the feature can be disabled to use explicit reads instead.

### Parallel Recovery Walk

On startup, the storage performs a parallel directory walk using multiple threads. Each thread processes subdirectories independently, emitting `(storage_name, creation_timestamp)` pairs to a recovery channel.

The walk applies several cleanup rules:
- Temporary files (`.tmp` extension) are deleted as incomplete writes
- Zero-length placeholder files are deleted
- Empty directories are removed after processing
- Files created after the walk started are ignored (they're new bundles being written concurrently)

The timestamp check prevents a race condition where concurrent writes could be mistaken for recovery candidates.

## Configuration

| Option | Default | Purpose |
|--------|---------|---------|
| `store_dir` | Platform-specific | Root directory for bundle storage |
| `fsync` | `true` | Enable fsync for crash safety (disable for hardware-backed durability) |

Default directories follow platform conventions: `~/.cache/` on Linux, `~/Library/Caches/` on macOS, `%LOCALAPPDATA%` on Windows.

## Integration

### With hardy-bpa

Implements the `BundleStorage` trait. The BPA calls `save()`, `load()`, `delete()`, and `recover()` without knowing the underlying storage mechanism.

### With hardy-bpa-server

The server instantiates localdisk-storage based on configuration and injects it into the BPA.

## Dependencies

| Crate | Purpose |
|-------|---------|
| hardy-bpa | `BundleStorage` trait definition |
| memmap2 | Memory-mapped file I/O (optional `mmap` feature) |
| directories | Platform-specific default paths |
| tokio | Async filesystem operations |

## Future Work

- **Capacity limits and eviction.** Configurable maximum storage capacity with eviction policies when capacity is reached, integrated with BPA's `storage_priority` metadata.

## Testing

- [Test Plan](test_plan.md) - Filesystem persistence verification
