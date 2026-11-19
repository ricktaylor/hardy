# hardy-localdisk-storage Design

Local filesystem bundle storage implementing the BundleStorage trait.

## Design Goals

- **Crash safety.** Bundle data must not be corrupted by power failure or process crash. Partial writes should never leave invalid state in storage.

- **Filesystem-friendly.** Avoid creating too many files in a single directory, which degrades performance on many filesystems. Distribute files across subdirectories.

- **Zero-copy loading.** Support memory-mapped file access where possible, allowing the kernel to handle caching and avoiding explicit read operations.

- **Parallel recovery.** On restart, scan the storage directory quickly using multiple threads to minimise startup time.

## Architecture Overview

The storage maps bundle identifiers to files on the local filesystem:

```mermaid
flowchart LR
    BPA --> save["save(bundle_data)"]
    BPA --> load["load(storage_name)"]
    BPA --> delete["delete(storage_name)"]
    BPA --> recover["recover()"]

    save --> claim["claim name (placeholder)"] --> tmp["write .tmp file"] --> fsync["fsync"] --> rename["atomic rename"] --> dirsync["fsync directory"]
    claim -. "fsync disabled" .-> direct["write through the claim"]
    load --> mmap["mmap file (or read)"]
    delete --> unlink["unlink file"]
    recover --> walk["parallel directory walk"] --> emit["emit (name, timestamp) pairs"]
```

Bundles are stored in a two-level directory hierarchy (256 x 256 directories) with random filenames. This distribution prevents any single directory from accumulating too many entries.

## Key Design Decisions

### Two-Level Directory Hierarchy

Many filesystems slow down significantly when directories contain thousands of files. The two-level structure (`ab/cd/12345678`) provides 65,536 possible parent directories, keeping individual directories small even with millions of bundles.

File names are random 32-bit hex values. On collision, the value is incremented until a unique name is found. The randomness avoids predictable patterns that might interact poorly with filesystem caching or create hot directories.

### Atomic Write Protocol

Writes follow a careful protocol to ensure crash safety:

1. Claim the final name with an exclusive create, which leaves a zero-length placeholder under it; where the claim had to create the bucket, `fsync` the two directories above it
2. Write bundle data to a temporary file (`.tmp` extension) beside the placeholder
3. `fsync` the temporary file to ensure data reaches disk
4. Atomic rename from temporary to final path, replacing the placeholder
5. `fsync` the bucket directory to ensure the rename is durable

This protocol means bundles are either fully present or fully absent. A crash before step 4 leaves a `.tmp` file and a zero-length placeholder, both of which recovery cleans up. A crash after step 4 leaves a complete bundle.

The placeholder is what makes the rename safe. A rename replaces whatever holds the destination name, so the name must already be this save's own: the exclusive create fails where a stored bundle or a concurrent save holds it, and the save moves to the next name. Renaming without the claim was rejected, since a name collision, however rare, would silently replace a stored bundle. Publishing with a hard link, which does refuse to replace, would avoid the placeholder but is unavailable on FAT, exFAT and some network filesystems.

A save that fails removes what it created. The placeholder and the temporary file are each removed on any error return, so a store that is failing saves, a full disk for instance, does not accumulate a zero-length file per attempt. A `.tmp` file found where the save's own belongs was left by an earlier process: its name derives from the name just claimed, which no live save shares, so it is removed and the save proceeds. Failing the save there was rejected, because the BPA treats a failed save as fatal, and one stale file would stop the node.

The bucket directory is created only when the claim finds it missing. A save into an existing bucket, the usual case once a store has run for a while, then makes no directory call at all. If the recovery walk removes an empty bucket between its creation and the claim, the claim creates it again.

The directory syncs are best effort: a failure is logged and the save stands, its data already synced, because some filesystems refuse to sync a directory. A new bucket is named by entries that are new too, its own and possibly its parent's, so the save that creates a bucket syncs the two directories above it, and does so straight after the claim: were it left to the end, a save that created the bucket and then failed would leave them unsynced for a later save that finds the bucket already there. Windows has no directory sync, so these steps are skipped there and the rename's durability is the filesystem's.

The `fsync` operations have performance cost. Configuration allows disabling them for deployments where the underlying storage provides its own durability guarantees (e.g., battery-backed RAID controllers, ZFS with synchronous writes). With `fsync` disabled, `save` skips the temporary file too: the exclusive create that claims the final name is also the file the bundle is written through, in one step, so the name is protected as in the atomic protocol and no file is opened twice. A crash can leave an empty or partial file under its final name. Nothing references it, because metadata is inserted only after `save` returns, and recovery discards it, the empty file as a placeholder and the partial one as unparseable.

An unsynced save checks only the write itself. An error the filesystem reports later, at write-back or as the file is closed (a network filesystem's deferred write, a quota), is not seen, because observing it takes the sync this mode exists to skip. A deployment that needs such errors reported keeps `fsync` enabled.

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

#### Internal flume + bridge pump

The walk runs inside `tokio::task::spawn_blocking` and uses `std::fs` (synchronous I/O), so it needs a blocking-friendly channel for emitting results. The trait surface, however, takes a `&dyn Sender<RecoveryResponse>` (see [bpa storage subsystem design](../../bpa/docs/storage_subsystem_design.md#streaming-results-via-sendert)) — async-only.

`recover()` bridges these by creating an internal `flume::bounded(parallelism * 16)`: the walk tasks `flume::Sender::send` into it (blocking, with `is_disconnected()` early-exit), and a sibling pump task in the same `recover()` future pulls from the flume receiver and forwards items to the external `Sender` via `send().await`. When the walk completes it drops its `flume_tx`, the pump's `recv_async` returns `Disconnected`, and the pump exits. When the external consumer goes away first, the pump's `stream.send` errors, the pump returns and drops `flume_rx`, and the walk's next `is_disconnected()` check stops the walk.

This keeps the existing walk code unchanged while presenting the `Sender`-shaped trait surface.

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

- **Spool-and-commit engine for streaming storage.** When the streaming `BundleStorage::store`/`load` traits land (see [streaming pipeline design](../../bpa/docs/streaming_pipeline_design.md)), the write path becomes a two-stage engine. *Spool stage:* one spool file per in-flight bundle, segments appended as they are pulled from the `Receiver`; backpressure propagates to the CLA through the pull model rather than a queue depth, and a small bundle arriving as a single segment degenerates to the current single-write path. *Commit stage:* completed spools enqueue to a commit queue serviced by one IO thread; commits are batched and grouped by parent directory — one fsync per data file plus coalesced directory fsyncs per batch — under a configurable durability ladder (none / data-only / full per-commit / batched-full). Bucket selection can deliberately bias commits within a batching window into the same `xx/yy` bucket to maximise directory-fsync coalescing. The existing tmp-write → rename commit is already the atomic spool commit the streaming design requires. The motivating problem is measurable today — each `save()` costs one `spawn_blocking` and an unamortised per-file fsync — but the rework should land with the streaming traits, not before, because batching whole-buffer writes would be thrown away by the switch to segment spooling.

## Testing

- [Test Plan](test_plan.md) - Filesystem persistence verification
