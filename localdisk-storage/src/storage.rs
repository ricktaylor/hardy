use std::{
    fs::{File, OpenOptions, create_dir_all, remove_file, rename},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::SystemTime,
};

use futures::join;
use hardy_bpa::{
    Bytes, async_trait,
    storage::{self, BundleStorage, RecoveryResponse},
    stream::Sender,
};
use rand::prelude::*;
use trace_err::*;
#[cfg(feature = "instrument")]
use tracing::instrument;
use tracing::{error, info, warn};

// Whether to use fsync for crash-safe atomic writes.
const DEFAULT_FSYNC: bool = true;

// Directory where bundle files are stored: a platform-specific cache
// directory resolved via the `directories` crate (e.g.
// `~/.cache/hardy-localdisk-storage` on Linux), falling back to
// `/var/spool/hardy-localdisk-storage` on Unix or the executable directory
// on Windows.
fn default_store_dir() -> PathBuf {
    directories::ProjectDirs::from("dtn", "Hardy", env!("CARGO_PKG_NAME")).map_or_else(
        || cfg_select! {
            unix => Path::new("/var/spool").join(env!("CARGO_PKG_NAME")),
            windows => std::env::current_exe().expect("Failed to get current exe").join(env!("CARGO_PKG_NAME")),
            _ => compile_error!("No idea how to determine default localdisk bundle store directory for target platform"),
        },
        |project_dirs| project_dirs.cache_dir().into(),
    )
}

/// Local-filesystem implementation of
/// [`BundleStorage`](hardy_bpa::storage::BundleStorage): bundles are
/// stored as individual files in a two-level hash directory layout.
pub struct LocalDiskStorage {
    store_root: PathBuf,
    fsync: bool,
}

impl LocalDiskStorage {
    /// Creates the store, ensuring the store directory exists (creating it
    /// if necessary). `None` applies the backend's own default: the
    /// platform cache directory, and fsync enabled (each save writes a
    /// `.tmp` file, syncs its data once the whole bundle is written,
    /// renames it to the final name, and syncs the parent directory;
    /// `Some(false)` writes the bundle straight to its final name, with no
    /// temporary file and no sync).
    pub fn new(store_dir: Option<PathBuf>, fsync: Option<bool>, _upgrade: bool) -> Self {
        let store_root = store_dir.unwrap_or_else(default_store_dir);
        info!("Using bundle store directory: {}", store_root.display());

        std::fs::create_dir_all(&store_root).trace_expect(&format!(
            "Failed to create bundle store directory {}",
            store_root.display()
        ));

        Self {
            store_root,
            fsync: fsync.unwrap_or(DEFAULT_FSYNC),
        }
    }
}

// A bundle's location under the store root: a random bucket in the
// two-level directory layout, and the file id the name search starts from.
struct Placement {
    dir: PathBuf,
    file_id: u64,
}

impl Placement {
    // Not a security value: the names only spread files across the buckets.
    fn random(root: &Path) -> Self {
        let mut rng = rand::rng();
        let dir1 = format!("{:02x}", rng.random::<u8>());
        let dir2 = format!("{:02x}", rng.random::<u8>());
        Self {
            dir: root.join(dir1).join(dir2),
            file_id: rng.random::<u32>() as u64,
        }
    }

    // Claims a file name no other bundle holds, creating the bucket if
    // necessary, and returns the new, empty file open for writing with its
    // path. The exclusive create is the claim: it fails where the name is
    // taken, by a stored bundle or by a save racing this one, and the search
    // moves on to the next id.
    fn create(mut self) -> io::Result<(File, PathBuf)> {
        create_dir_all(&self.dir)?;
        loop {
            let file_path = self.dir.join(format!("{:x}", self.file_id));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file_path)
            {
                Ok(file) => return Ok((file, file_path)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    self.file_id = self.file_id.wrapping_add(1);
                }
                Err(e) => return Err(e),
            }
        }
    }
}

// The fsync-enabled save: write a temporary file beside a placeholder that
// holds the final name, sync it, rename it over the placeholder, and sync
// the directory.
#[cfg_attr(feature = "instrument", instrument(skip_all))]
fn save_atomic(placement: Placement, data: &[u8]) -> io::Result<PathBuf> {
    // The zero-length placeholder holds the name until the rename replaces
    // it: a rename cannot refuse to overwrite, so the name must be this
    // save's own before the bundle is published under it.
    let (_, mut storage_name) = placement.create()?;

    // Use a temporary extension
    storage_name.set_extension("tmp");

    // Buffered writes: the single `sync_data` below makes the whole bundle
    // durable at once, where a write-through open would flush the device on
    // every write call as well.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&storage_name)?;

    // Write all data to file
    file.write_all(data).inspect_err(|e| {
        error!("Failed to write bundle data: {e}");
        _ = remove_file(&storage_name);
    })?;

    // Sync the data before the rename publishes it (the directory is synced
    // after the rename)
    file.sync_data().inspect_err(|e| {
        error!("Failed to sync bundle file data: {e}");
        _ = remove_file(&storage_name);
    })?;

    // Rename the file
    let old_path = storage_name.clone();
    storage_name.set_extension("");
    rename(&old_path, &storage_name).inspect_err(|e| {
        error!("Failed to rename temporary bundle data file to final name: {e}");
        _ = remove_file(&old_path);
    })?;

    // And now sync the parent directory, i.e. metadata
    if let Some(parent_dir) = storage_name.parent()
        && let Err(e) = File::open(parent_dir).and_then(|f| f.sync_all())
    {
        warn!("Failed to sync parent directory: {e}");
    }

    Ok(storage_name)
}

// The save without fsync: claim the final name and write the bundle through
// the handle that claimed it, in one step. A crash can leave the file empty
// or partial under its final name; no metadata references it yet, and
// recovery discards it.
#[cfg_attr(feature = "instrument", instrument(skip_all))]
fn save_direct(placement: Placement, data: &[u8]) -> io::Result<PathBuf> {
    let (mut file, storage_name) = placement.create()?;
    file.write_all(data).inspect_err(|e| {
        error!("Failed to write bundle data: {e}");
        _ = remove_file(&storage_name);
    })?;
    Ok(storage_name)
}

#[cfg_attr(feature = "instrument", instrument(skip(tx)))]
fn walk_dirs(
    before: &SystemTime,
    root: &PathBuf,
    dir: PathBuf,
    tx: &flume::Sender<RecoveryResponse>,
) -> Vec<PathBuf> {
    let mut subdirs = Vec::new();
    if let Ok(dir) = std::fs::read_dir(dir.clone()) {
        for entry in dir.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    subdirs.push(entry.path());
                } else if file_type.is_file() {
                    // There is a race during restart: bundles may expire, concurrent
                    // save() operations may be in progress, so file state can change.
                    // It is valid for the file to no longer exist.

                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };

                    // Prefer creation time, fall back to modification time
                    // (some filesystems like older ext4 don't track creation time)
                    let Ok(file_time) = metadata.created().or_else(|_| metadata.modified()) else {
                        warn!("Failed to get timestamp for {}", entry.path().display());
                        continue;
                    };

                    // Skip anything created after we began our walk - these are new
                    // bundles being saved concurrently, not recovery candidates
                    if &file_time > before {
                        continue;
                    }

                    // Drop .tmp files left by interrupted save()
                    if let Some(extension) = entry.path().extension()
                        && extension == "tmp"
                    {
                        if let Err(e) = std::fs::remove_file(entry.path())
                            && e.kind() != std::io::ErrorKind::NotFound
                        {
                            // NotFound is benign (concurrent save() or reaper removed it)
                            warn!("Failed to remove tmp file {}: {e}", entry.path().display());
                        }
                        continue;
                    }

                    // Drop 0-length placeholder files left by interrupted save()
                    if metadata.len() == 0 {
                        if let Err(e) = std::fs::remove_file(entry.path())
                            && e.kind() != std::io::ErrorKind::NotFound
                        {
                            // NotFound is benign (concurrent save() completed and overwrote it)
                            warn!(
                                "Failed to remove placeholder {}: {e}",
                                entry.path().display()
                            );
                        }
                        continue;
                    }

                    if tx
                        .send((
                            entry
                                .path()
                                .strip_prefix(root)
                                .trace_expect("Failed to strip prefix?!")
                                .to_string_lossy()
                                .into(),
                            time::OffsetDateTime::from(file_time),
                        ))
                        .is_err()
                    {
                        // Exit fast
                        return Vec::new();
                    }
                }
            }
        }
    }

    // Try to remove the directory - this will benignly fail if there is content
    _ = std::fs::remove_dir(&dir);

    subdirs
}

#[async_trait]
impl BundleStorage for LocalDiskStorage {
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn recover(&self, stream: &dyn Sender<RecoveryResponse>) -> storage::Result<()> {
        // Internal flume channel: walk_dirs uses blocking send + is_disconnected
        // (called from spawn_blocking), so we keep flume internally and bridge
        // to the external Sender via a sibling pump task.
        let parallelism: usize = std::thread::available_parallelism()
            .map(Into::into)
            .unwrap_or(1);
        let (flume_tx, flume_rx) = flume::bounded(parallelism * 16);

        // Walk: existing logic unchanged, operating on the internal flume.
        let walk = async {
            let before = SystemTime::now();
            let mut dirs = vec![self.store_root.clone()];
            let mut task_set = tokio::task::JoinSet::new();
            let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism));

            // Loop through the directories
            while !dirs.is_empty() && !flume_tx.is_disconnected() {
                // Take a chunk off the back, to ensure depth first walk
                let subdirs = dirs.split_off(dirs.len() - dirs.len().min(32));

                loop {
                    tokio::select! {
                        // Throttle the number of threads
                        permit = semaphore.clone().acquire_owned() => {
                            let permit = permit.trace_expect("Failed to acquire permit");
                            let root = self.store_root.clone();
                            let tx = flume_tx.clone();
                            task_set.spawn_blocking(move || {
                                let mut dirs = Vec::new();
                                for dir in subdirs {
                                    dirs.extend(walk_dirs(&before,&root, dir, &tx));
                                }
                                drop(permit);
                                dirs
                            });
                            break;
                        },
                        // Collect results
                        Some(r) = task_set.join_next(), if !task_set.is_empty() => {
                            dirs.extend(r.trace_expect("Task terminated unexpectedly"));
                        }
                    }
                }

                while dirs.is_empty() || flume_tx.is_disconnected() {
                    // Accumulate results
                    let Some(r) = task_set.join_next().await else {
                        break;
                    };
                    dirs.extend(r.trace_expect("Task terminated unexpectedly"));
                }
            }
            // Drop our handle so the pump sees disconnect once the buffer drains.
            drop(flume_tx);
        };

        // Pump: forward items from the internal flume to the external Sender.
        let pump = async {
            loop {
                match flume_rx.recv_async().await {
                    Ok(item) => {
                        if stream.send(item).await.is_err() {
                            // External consumer gone — return so flume_rx drops
                            // and walk's is_disconnected check fires.
                            return;
                        }
                    }
                    Err(_) => return, // Walk done and buffer drained.
                }
            }
        };

        join!(walk, pump);
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn load(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        let storage_name = self.store_root.join(PathBuf::from_str(storage_name)?);

        cfg_select! {
            feature = "mmap" => {
                let file = match tokio::fs::File::open(storage_name).await {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(None);
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                    Ok(file) => file,
                };
                let data = unsafe { memmap2::Mmap::map(&file) };
                Ok(Some(Bytes::from_owner(data?)))
            }
            _ => {
                tokio::fs::read(storage_name)
                    .await
                    .map(|data| Some(Bytes::from_owner(data)))
                    .or_else(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => Ok(None),
                        _ => Err(e.into()),
                    })
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn save(&self, data: Bytes) -> storage::Result<Arc<str>> {
        let placement = Placement::random(&self.store_root);
        let fsync = self.fsync;
        let storage_name = tokio::task::spawn_blocking(move || {
            if fsync {
                save_atomic(placement, &data)
            } else {
                save_direct(placement, &data)
            }
        })
        .await
        .trace_expect("Failed to spawn bundle save thread")?;

        Ok(storage_name
            .strip_prefix(&self.store_root)?
            .to_string_lossy()
            .into())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn delete(&self, storage_name: &str) -> storage::Result<()> {
        tokio::fs::remove_file(&self.store_root.join(PathBuf::from_str(storage_name)?))
            .await
            .or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    warn!("Failed to remove {storage_name}");
                    Ok(())
                }
                _ => Err(e.into()),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use hardy_bpa::{
        Bytes,
        storage::BundleStorage,
        stream::{SendError, Sender},
    };

    use super::{LocalDiskStorage, Placement, save_atomic, save_direct};

    /// Test sink that collects items into a `Vec` for assertions.
    struct VecSink<T>(std::sync::Mutex<Vec<T>>);

    impl<T> VecSink<T> {
        fn new() -> Self {
            Self(std::sync::Mutex::new(Vec::new()))
        }

        fn into_inner(self) -> Vec<T> {
            self.0.into_inner().unwrap()
        }
    }

    #[hardy_bpa::async_trait]
    impl<T: Send + Sync + 'static> Sender<T> for VecSink<T> {
        async fn send(&self, item: T) -> Result<(), SendError<T>> {
            self.0.lock().unwrap().push(item);
            Ok(())
        }
    }

    // LD-01: Files are created under the configured store_dir.
    #[tokio::test]
    async fn test_configuration_custom_store_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

        let name = store.save(Bytes::from_static(b"hello")).await.unwrap();
        let full_path = dir.path().join(&*name);
        assert!(
            full_path.exists(),
            "bundle file should exist under configured store_dir"
        );
    }

    // LD-02: Recovery cleans up .tmp files, zero-byte placeholders, and empty dirs.
    #[tokio::test]
    async fn test_recovery_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

        // Save a valid bundle
        let valid_name = store.save(Bytes::from_static(b"valid")).await.unwrap();

        // Create a .tmp file (interrupted save)
        let tmp_dir = dir.path().join("aa").join("bb");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let tmp_path = tmp_dir.join("deadbeef.tmp");
        std::fs::write(&tmp_path, b"partial").unwrap();

        // Create a zero-byte placeholder (interrupted save)
        let placeholder_path = tmp_dir.join("00000000");
        std::fs::File::create(&placeholder_path).unwrap();

        // Create an empty subdirectory
        let empty_dir = dir.path().join("cc").join("dd");
        std::fs::create_dir_all(&empty_dir).unwrap();

        // Run recovery
        let sink = VecSink::new();
        store.recover(&sink).await.unwrap();

        // Collect recovered entries
        let recovered = sink.into_inner();

        // Only the valid bundle should be recovered
        assert_eq!(recovered.len(), 1, "should recover exactly 1 valid bundle");
        assert_eq!(&*recovered[0].0, &*valid_name);

        // .tmp file should be cleaned up
        assert!(!tmp_path.exists(), ".tmp file should be removed");

        // Zero-byte placeholder should be cleaned up
        assert!(
            !placeholder_path.exists(),
            "zero-byte placeholder should be removed"
        );

        // Empty directory should be cleaned up (inner `dd` removed, `cc` may also be removed)
        assert!(!empty_dir.exists(), "empty directory should be removed");
    }

    // LD-03: Files are distributed in a two-level xx/yy/ directory structure.
    #[tokio::test]
    async fn test_filesystem_structure() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

        // Save several bundles
        let mut names = Vec::new();
        for _ in 0..5 {
            names.push(store.save(Bytes::from_static(b"data")).await.unwrap());
        }

        for name in &names {
            let parts: Vec<&str> = name.split(std::path::MAIN_SEPARATOR).collect();
            assert_eq!(
                parts.len(),
                3,
                "storage name '{name}' should have 3 path components (xx/yy/file)"
            );
            // First two components should be 2-char hex directories
            assert_eq!(
                parts[0].len(),
                2,
                "first dir component should be 2 hex chars"
            );
            assert_eq!(
                parts[1].len(),
                2,
                "second dir component should be 2 hex chars"
            );
            assert!(
                u8::from_str_radix(parts[0], 16).is_ok(),
                "first dir component '{}' should be valid hex",
                parts[0]
            );
            assert!(
                u8::from_str_radix(parts[1], 16).is_ok(),
                "second dir component '{}' should be valid hex",
                parts[1]
            );
        }
    }

    // LD-04: With fsync=true, save writes to .tmp then renames (no .tmp left behind).
    #[tokio::test]
    async fn test_atomic_save_no_tmp_residue() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(true), false);

        let name = store.save(Bytes::from_static(b"atomic")).await.unwrap();

        // The final file should exist
        let full_path = dir.path().join(&*name);
        assert!(full_path.exists(), "final file should exist");

        // No .tmp files should remain anywhere
        let has_tmp = walkdir(dir.path())
            .iter()
            .any(|p| p.extension().is_some_and(|e| e == "tmp"));
        assert!(!has_tmp, "no .tmp files should remain after save");

        // Data should be correct
        let loaded = store.load(&name).await.unwrap().unwrap();
        assert_eq!(&*loaded, b"atomic");
    }

    // LD-05: Save on a read-only directory returns an error, not a panic.
    #[tokio::test]
    async fn test_save_to_readonly_dir_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

        // Make the store directory read-only
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(dir.path(), perms.clone()).unwrap();

        // save() should return an error, not panic
        let result = store.save(Bytes::from_static(b"fail")).await;
        assert!(result.is_err(), "save to read-only dir should return Err");

        // Restore permissions so tempdir cleanup succeeds
        cfg_select! {
            unix => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            _ => {
                perms.set_readonly(false);
                std::fs::set_permissions(dir.path(), perms).unwrap();
            }
        }
    }

    // LD-06: A save whose first-choice name is taken, by a stored bundle or
    // by a save still in progress, moves to the next name: the existing file
    // is never overwritten. Both modes claim their name the same way.
    #[test]
    fn test_save_never_overwrites_an_existing_name() {
        for fsync in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let bucket = dir.path().join("ab").join("cd");
            let placement = || Placement {
                dir: bucket.clone(),
                file_id: 0x1234,
            };
            let save = |data: &[u8]| {
                if fsync {
                    save_atomic(placement(), data)
                } else {
                    save_direct(placement(), data)
                }
            };

            let first = save(b"first").unwrap();
            assert_eq!(first, bucket.join("1234"), "fsync={fsync}");

            // The same placement again: the name is taken.
            let second = save(b"second").unwrap();
            assert_eq!(second, bucket.join("1235"), "fsync={fsync}");
            assert_eq!(std::fs::read(&first).unwrap(), b"first", "fsync={fsync}");
            assert_eq!(std::fs::read(&second).unwrap(), b"second", "fsync={fsync}");

            // No placeholder or temporary file is left beside the two bundles.
            assert_eq!(walkdir(dir.path()).len(), 2, "fsync={fsync}");
        }
    }

    // LD-07: With fsync=false, save writes the bundle under its final name
    // in one step: no `.tmp` file is created, and the data round-trips.
    #[tokio::test]
    async fn test_direct_save_writes_final_name_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

        let name = store.save(Bytes::from_static(b"direct")).await.unwrap();

        assert_eq!(
            walkdir(dir.path()),
            [dir.path().join(&*name)],
            "the bundle's file is the only file in the store"
        );
        let loaded = store.load(&name).await.unwrap().unwrap();
        assert_eq!(&*loaded, b"direct");
    }

    // Recursively collect all file paths under a directory.
    fn walkdir(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files.extend(walkdir(&path));
                } else {
                    files.push(path);
                }
            }
        }
        files
    }
}
