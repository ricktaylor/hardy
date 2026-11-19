use std::{
    fs::{File, OpenOptions, create_dir_all, read_dir, remove_dir, remove_file, rename},
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
use tokio::task::spawn_blocking;
use trace_err::*;
#[cfg(feature = "instrument")]
use tracing::instrument;
use tracing::{error, info, warn};

// Whether to use fsync for crash-safe atomic writes.
const DEFAULT_FSYNC: bool = true;

// How many times one claim creates its bucket before the save fails. The
// first creation is the ordinary one, for a bucket that does not exist yet.
// Only the recovery walk removes directories, and it visits each once, so it
// can take a bucket from under a claim once: the limit allows that and one
// more. A bucket still missing after that is not the walk's doing, and the
// claim reports it instead of creating the bucket without end.
const BUCKET_CREATIONS: u32 = 3;

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
    /// platform cache directory, and fsync enabled (each save claims its
    /// final name with a zero-length placeholder, writes a `.tmp` file
    /// beside it, syncs its data once the whole bundle is written, renames
    /// it over the placeholder, and syncs the directory; `Some(false)`
    /// writes the bundle straight to the name it claims, with no temporary
    /// file and no sync).
    pub fn new(store_dir: Option<PathBuf>, fsync: Option<bool>, _upgrade: bool) -> Self {
        let store_root = store_dir.unwrap_or_else(default_store_dir);
        info!("Using bundle store directory: {}", store_root.display());

        create_dir_all(&store_root).trace_expect(&format!(
            "Failed to create bundle store directory {}",
            store_root.display()
        ));

        Self {
            store_root,
            fsync: fsync.unwrap_or(DEFAULT_FSYNC),
        }
    }

    // Saves `data` at `placement` in this store's fsync mode, off the
    // runtime's worker threads. `save` chooses the placement at random; a
    // test fixes it, to save under a known name.
    async fn save_at(&self, placement: Placement, data: Bytes) -> storage::Result<Arc<str>> {
        let fsync = self.fsync;
        let storage_name = spawn_blocking(move || save_in_mode(fsync, &placement, &data))
            .await
            .trace_expect("Failed to spawn bundle save thread")?;

        Ok(storage_name
            .strip_prefix(&self.store_root)?
            .to_string_lossy()
            .into())
    }
}

// A test's hook on a bucket's creation: called with the bucket's path. It
// counts the creations, so it is a shared closure, not a plain function.
#[cfg(test)]
type BucketHook = Arc<dyn Fn(&Path) + Send + Sync>;

// A bundle's location under the store root: a random bucket in the
// two-level directory layout, and the file id the name search starts from.
struct Placement {
    dir: PathBuf,
    file_id: u64,
    // Called with the claimed path once a claim succeeds, so a test can act
    // between the claim and the write that follows it.
    #[cfg(test)]
    after_claim: Option<fn(&Path)>,
    // Called with the bucket each time a claim creates it, so a test can
    // remove it again, as the recovery walk removes an empty directory.
    #[cfg(test)]
    after_bucket_created: Option<BucketHook>,
}

// A name claimed for one bundle: the new, empty file open for writing, its
// path, and whether claiming it created the bucket.
struct Claim {
    file: File,
    path: PathBuf,
    created_bucket: bool,
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
            #[cfg(test)]
            after_claim: None,
            #[cfg(test)]
            after_bucket_created: None,
        }
    }

    // Claims a file name no other bundle holds. The exclusive create is the
    // claim: it fails where the name is taken, by a stored bundle or by a
    // save racing this one, and the search moves on to the next id.
    //
    // The bucket is created only when the create finds it missing, so a save
    // into an existing bucket makes no directory call. `NotFound` again
    // after creating it means the recovery walk removed the empty bucket in
    // between, and the claim creates it again, up to `BUCKET_CREATIONS`
    // times: past that the `NotFound` is returned, so both retries end. The
    // name search ends at the first free id, the bucket retry at its limit.
    fn claim(&self) -> io::Result<Claim> {
        let mut file_id = self.file_id;
        let mut bucket_creations = 0;
        loop {
            let path = self.dir.join(format!("{file_id:x}"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    #[cfg(test)]
                    if let Some(after_claim) = self.after_claim {
                        after_claim(&path);
                    }
                    return Ok(Claim {
                        file,
                        path,
                        created_bucket: bucket_creations > 0,
                    });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    file_id = file_id.wrapping_add(1);
                }
                Err(e)
                    if e.kind() == ErrorKind::NotFound && bucket_creations < BUCKET_CREATIONS =>
                {
                    create_dir_all(&self.dir)?;
                    bucket_creations += 1;
                    #[cfg(test)]
                    if let Some(after_bucket_created) = &self.after_bucket_created {
                        after_bucket_created(&self.dir);
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

// Removes a file a save has created unless the save completes, so an error
// from any step leaves no placeholder, temporary or partial file behind.
struct RemoveOnDrop(Option<PathBuf>);

impl RemoveOnDrop {
    fn new(path: &Path) -> Self {
        Self(Some(path.to_path_buf()))
    }

    // The file is the save's result, or has been renamed away: keep it.
    fn keep(mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            _ = remove_file(path);
        }
    }
}

// Syncs a directory, so the entries created or renamed in it survive a
// crash. Best effort: some filesystems refuse to sync a directory, so a
// failure is logged and the save stands, its data already synced.
fn sync_dir(dir: &Path) {
    cfg_select! {
        windows => {
            // Windows has no directory sync, and `File::open` cannot open a
            // directory there: the rename's durability is the filesystem's.
            _ = dir;
        }
        _ => {
            if let Err(e) = File::open(dir).and_then(|dir| dir.sync_all()) {
                warn!("Failed to sync directory {}: {e}", dir.display());
            }
        }
    }
}

// Creates a save's temporary file, exclusively. Its name derives from the
// final name the save has just claimed, so no live save holds it: one found
// here was left by an earlier process, its placeholder already swept, and is
// removed first.
fn create_tmp(tmp_name: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    match options.open(tmp_name) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            remove_file(tmp_name)?;
            options.open(tmp_name)
        }
        result => result,
    }
}

// The fsync-enabled save: write a temporary file beside a placeholder that
// holds the final name, sync it, rename it over the placeholder, and sync
// the directory.
#[cfg_attr(feature = "instrument", instrument(skip_all))]
fn save_atomic(placement: &Placement, data: &[u8]) -> io::Result<PathBuf> {
    // The zero-length placeholder holds the name until the rename replaces
    // it: a rename cannot refuse to overwrite, so the name must be this
    // save's own before the bundle is published under it. Nothing writes
    // through the claiming handle, so it is closed at once, not held open
    // across the rename.
    let Claim {
        file,
        path: storage_name,
        created_bucket,
    } = placement.claim()?;
    drop(file);
    let placeholder = RemoveOnDrop::new(&storage_name);

    // A bucket this save created is named by entries that are new too: its
    // own, and perhaps its parent's. Sync the two directories above it now,
    // so they are durable even if this save fails and a later one, finding
    // the bucket already there, syncs only the bucket.
    if created_bucket {
        for ancestor in placement.dir.ancestors().skip(1).take(2) {
            sync_dir(ancestor);
        }
    }

    let tmp_name = storage_name.with_extension("tmp");
    let mut file = create_tmp(&tmp_name)
        .inspect_err(|e| error!("Failed to create temporary bundle data file: {e}"))?;
    let tmp = RemoveOnDrop::new(&tmp_name);

    // Write-back, not write-through: the data lands in the page cache and
    // the single `sync_data` below makes it durable, rather than `O_SYNC`
    // forcing a device flush at the end of each `write(2)`. `File` has no
    // buffer of its own, so nothing is left unwritten when it is synced.
    file.write_all(data)
        .inspect_err(|e| error!("Failed to write bundle data: {e}"))?;

    // Sync the data before the rename publishes it (the directory is synced
    // after the rename)
    file.sync_data()
        .inspect_err(|e| error!("Failed to sync bundle file data: {e}"))?;

    rename(&tmp_name, &storage_name).inspect_err(|e| {
        error!("Failed to rename temporary bundle data file to final name: {e}");
    })?;
    // The rename consumed the temporary file and replaced the placeholder
    // with the bundle.
    tmp.keep();
    placeholder.keep();

    // And now sync the bucket, so the rename is durable.
    sync_dir(&placement.dir);

    Ok(storage_name)
}

// The save without fsync: claim the final name and write the bundle through
// the handle that claimed it, in one step, never opening the name again. A
// crash can leave the file empty or partial under its final name; no
// metadata references it yet, and recovery discards it.
//
// Only the write itself is checked. An error the filesystem reports later,
// at write-back or as the file is closed (a network filesystem's deferred
// write, a quota), is not seen: observing it takes the sync this mode
// exists to skip, and `File` reports no error from its close.
#[cfg_attr(feature = "instrument", instrument(skip_all))]
fn save_unsynced(placement: &Placement, data: &[u8]) -> io::Result<PathBuf> {
    let Claim {
        mut file,
        path: storage_name,
        ..
    } = placement.claim()?;
    let partial = RemoveOnDrop::new(&storage_name);
    file.write_all(data)
        .inspect_err(|e| error!("Failed to write bundle data: {e}"))?;
    partial.keep();
    Ok(storage_name)
}

// Saves `data` at `placement`: atomically where the store syncs, otherwise
// in one unsynced write.
fn save_in_mode(fsync: bool, placement: &Placement, data: &[u8]) -> io::Result<PathBuf> {
    if fsync {
        save_atomic(placement, data)
    } else {
        save_unsynced(placement, data)
    }
}

#[cfg_attr(feature = "instrument", instrument(skip(tx)))]
fn walk_dirs(
    before: &SystemTime,
    root: &PathBuf,
    dir: PathBuf,
    tx: &flume::Sender<RecoveryResponse>,
) -> Vec<PathBuf> {
    let mut subdirs = Vec::new();
    if let Ok(dir) = read_dir(dir.clone()) {
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
                        if let Err(e) = remove_file(entry.path())
                            && e.kind() != ErrorKind::NotFound
                        {
                            // NotFound is benign (concurrent save() or reaper removed it)
                            warn!("Failed to remove tmp file {}: {e}", entry.path().display());
                        }
                        continue;
                    }

                    // Drop 0-length placeholder files left by interrupted save()
                    if metadata.len() == 0 {
                        if let Err(e) = remove_file(entry.path())
                            && e.kind() != ErrorKind::NotFound
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
    _ = remove_dir(&dir);

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
                    Err(e) if e.kind() == ErrorKind::NotFound => {
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
                        ErrorKind::NotFound => Ok(None),
                        _ => Err(e.into()),
                    })
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn save(&self, data: Bytes) -> storage::Result<Arc<str>> {
        self.save_at(Placement::random(&self.store_root), data)
            .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn delete(&self, storage_name: &str) -> storage::Result<()> {
        tokio::fs::remove_file(&self.store_root.join(PathBuf::from_str(storage_name)?))
            .await
            .or_else(|e| match e.kind() {
                ErrorKind::NotFound => {
                    warn!("Failed to remove {storage_name}");
                    Ok(())
                }
                _ => Err(e.into()),
            })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{read, set_permissions, write},
        path::MAIN_SEPARATOR,
        sync::{
            Barrier, Mutex,
            atomic::{AtomicU32, Ordering},
        },
        thread,
    };

    use hardy_bpa::stream::SendError;
    use tempfile::{TempDir, tempdir};

    use super::*;

    /// Test sink that collects items into a `Vec` for assertions.
    struct VecSink<T>(Mutex<Vec<T>>);

    impl<T> VecSink<T> {
        fn new() -> Self {
            Self(Mutex::new(Vec::new()))
        }

        fn into_inner(self) -> Vec<T> {
            self.0.into_inner().unwrap()
        }
    }

    #[async_trait]
    impl<T: Send + Sync + 'static> Sender<T> for VecSink<T> {
        async fn send(&self, item: T) -> Result<(), SendError<T>> {
            self.0.lock().unwrap().push(item);
            Ok(())
        }
    }

    // A store over a fresh temporary directory, in the given fsync mode.
    fn new_store(fsync: bool) -> (TempDir, LocalDiskStorage) {
        let dir = tempdir().unwrap();
        let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(fsync), false);
        (dir, store)
    }

    // The bucket every fixed placement uses.
    fn bucket(root: &Path) -> PathBuf {
        root.join("ab").join("cd")
    }

    // A fixed placement under `root`, so a test knows the name a save claims
    // first: `ab/cd/1234`.
    fn placement(root: &Path) -> Placement {
        Placement {
            dir: bucket(root),
            file_id: 0x1234,
            after_claim: None,
            after_bucket_created: None,
        }
    }

    // A placement whose bucket is removed the first `removals` times a claim
    // creates it, as the recovery walk removes an empty directory, with the
    // count of creations.
    fn placement_losing_its_bucket(root: &Path, removals: u32) -> (Placement, Arc<AtomicU32>) {
        let creations = Arc::new(AtomicU32::new(0));
        let counted = creations.clone();
        let placement = Placement {
            after_bucket_created: Some(Arc::new(move |bucket: &Path| {
                if counted.fetch_add(1, Ordering::Relaxed) < removals {
                    remove_dir(bucket).unwrap();
                }
            })),
            ..placement(root)
        };
        (placement, creations)
    }

    // Every file under `dir`, sorted. A directory that cannot be read fails
    // the test, so an assertion about what is absent cannot pass vacuously.
    fn walkdir(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(walkdir(&path));
            } else {
                files.push(path);
            }
        }
        files.sort();
        files
    }

    // LD-01: Files are created under the configured store_dir.
    #[tokio::test]
    async fn test_configuration_custom_store_dir() {
        let (dir, store) = new_store(false);

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
        let (dir, store) = new_store(false);

        // Save a valid bundle
        let valid_name = store.save(Bytes::from_static(b"valid")).await.unwrap();

        // Create a .tmp file (interrupted save)
        let tmp_dir = dir.path().join("aa").join("bb");
        create_dir_all(&tmp_dir).unwrap();
        let tmp_path = tmp_dir.join("deadbeef.tmp");
        write(&tmp_path, b"partial").unwrap();

        // Create a zero-byte placeholder (interrupted save)
        let placeholder_path = tmp_dir.join("00000000");
        File::create(&placeholder_path).unwrap();

        // Create an empty subdirectory
        let empty_dir = dir.path().join("cc").join("dd");
        create_dir_all(&empty_dir).unwrap();

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
        let (_dir, store) = new_store(false);

        // Save several bundles
        let mut names = Vec::new();
        for _ in 0..5 {
            names.push(store.save(Bytes::from_static(b"data")).await.unwrap());
        }

        for name in &names {
            let parts: Vec<&str> = name.split(MAIN_SEPARATOR).collect();
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
        let (dir, store) = new_store(true);

        let name = store.save(Bytes::from_static(b"atomic")).await.unwrap();

        // The bundle's file is the only file: no .tmp or placeholder remains
        assert_eq!(walkdir(dir.path()), [dir.path().join(&*name)]);

        // Data should be correct
        let loaded = store.load(&name).await.unwrap().unwrap();
        assert_eq!(&*loaded, b"atomic");
    }

    // LD-05: Save on a read-only directory returns the directory's refusal,
    // not a panic.
    #[tokio::test]
    async fn test_save_to_readonly_dir_returns_error() {
        let (dir, store) = new_store(false);

        // Make the store directory read-only
        let writable = dir.path().metadata().unwrap().permissions();
        let mut read_only = writable.clone();
        read_only.set_readonly(true);
        set_permissions(dir.path(), read_only).unwrap();

        let result = store.save(Bytes::from_static(b"fail")).await;

        // Restore permissions so tempdir cleanup succeeds
        set_permissions(dir.path(), writable).unwrap();

        let error = result.expect_err("save to a read-only directory fails");
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(ErrorKind::PermissionDenied)
        );
    }

    // LD-06: A save whose first-choice name is taken moves to the next name:
    // the stored bundle is never overwritten. Both modes claim their name
    // the same way.
    #[tokio::test]
    async fn test_save_never_overwrites_an_existing_name() {
        for fsync in [false, true] {
            let (dir, store) = new_store(fsync);
            let root = dir.path();

            let first = store
                .save_at(placement(root), Bytes::from_static(b"first"))
                .await
                .unwrap();
            // The same placement again: the name is taken.
            let second = store
                .save_at(placement(root), Bytes::from_static(b"second"))
                .await
                .unwrap();

            let (first, second) = (root.join(&*first), root.join(&*second));
            assert_eq!(
                [&first, &second],
                [&bucket(root).join("1234"), &bucket(root).join("1235")],
                "fsync={fsync}"
            );
            assert_eq!(read(&first).unwrap(), b"first", "fsync={fsync}");
            assert_eq!(read(&second).unwrap(), b"second", "fsync={fsync}");

            // No placeholder or temporary file is left beside the two bundles.
            assert_eq!(walkdir(root), [first, second], "fsync={fsync}");
        }
    }

    // LD-07: With fsync=false, save writes the bundle under its final name
    // in one step: no `.tmp` file is created, and the data round-trips.
    #[tokio::test]
    async fn test_unsynced_save_writes_final_name_only() {
        let (dir, store) = new_store(false);

        let name = store.save(Bytes::from_static(b"direct")).await.unwrap();

        assert_eq!(
            walkdir(dir.path()),
            [dir.path().join(&*name)],
            "the bundle's file is the only file in the store"
        );
        let loaded = store.load(&name).await.unwrap().unwrap();
        assert_eq!(&*loaded, b"direct");
    }

    // LD-08: A `.tmp` file left beside a free name by an earlier process does
    // not stop a save, and shows which path the store's mode takes. With
    // fsync on, the save's own temporary file replaces it and is renamed
    // away; with fsync off, the save uses no temporary file and leaves it.
    #[tokio::test]
    async fn test_stale_tmp_file_does_not_block_a_save() {
        for (fsync, left) in [(true, vec!["1234"]), (false, vec!["1234", "1234.tmp"])] {
            let (dir, store) = new_store(fsync);
            let root = dir.path();
            create_dir_all(bucket(root)).unwrap();
            write(bucket(root).join("1234.tmp"), b"stale").unwrap();

            let name = store
                .save_at(placement(root), Bytes::from_static(b"bundle"))
                .await
                .unwrap();

            assert_eq!(
                root.join(&*name),
                bucket(root).join("1234"),
                "fsync={fsync}"
            );
            assert_eq!(
                read(bucket(root).join("1234")).unwrap(),
                b"bundle",
                "fsync={fsync}"
            );
            let left: Vec<_> = left.iter().map(|file| bucket(root).join(file)).collect();
            assert_eq!(walkdir(root), left, "fsync={fsync}");
        }
    }

    // LD-09: A save that fails after claiming its name leaves no placeholder
    // behind. A directory where the temporary file belongs fails the atomic
    // save there: it cannot be replaced as a stale file is.
    #[test]
    fn test_failed_atomic_save_leaves_nothing_behind() {
        let dir = tempdir().unwrap();
        create_dir_all(bucket(dir.path()).join("1234.tmp")).unwrap();

        let error = save_in_mode(true, &placement(dir.path()), b"bundle")
            .expect_err("a directory in the temporary file's place fails the save");

        // Removing a directory as a file is refused, differently per platform.
        let refusal = cfg_select! {
            target_os = "linux" => ErrorKind::IsADirectory,
            _ => ErrorKind::PermissionDenied,
        };
        assert_eq!(error.kind(), refusal);
        assert_eq!(
            walkdir(dir.path()),
            Vec::<PathBuf>::new(),
            "the placeholder is removed"
        );
    }

    // LD-10: Saves racing for one name each claim their own. Every thread
    // starts from the same placement behind one barrier, into a bucket that
    // does not exist yet; whatever the interleaving, each bundle ends under
    // a distinct name with its own bytes.
    #[test]
    fn test_concurrent_saves_claim_distinct_names() {
        const SAVES: u64 = 16;
        for fsync in [false, true] {
            let dir = tempdir().unwrap();
            let root = dir.path();
            let barrier = Barrier::new(SAVES as usize);

            let saved: Vec<(PathBuf, Vec<u8>)> = thread::scope(|scope| {
                let saves: Vec<_> = (0..SAVES)
                    .map(|i| {
                        let barrier = &barrier;
                        scope.spawn(move || {
                            let data = format!("bundle {i}").into_bytes();
                            barrier.wait();
                            let path = save_in_mode(fsync, &placement(root), &data).unwrap();
                            (path, data)
                        })
                    })
                    .collect();
                saves.into_iter().map(|save| save.join().unwrap()).collect()
            });

            // Each save found its own bytes under the name it was given...
            for (path, data) in &saved {
                assert_eq!(&read(path).unwrap(), data, "fsync={fsync}");
            }
            // ...and the names are the first SAVES ids, each claimed once,
            // with no placeholder or temporary file left beside them.
            let names: Vec<_> = (0..SAVES)
                .map(|i| bucket(root).join(format!("{:x}", 0x1234 + i)))
                .collect();
            let mut claimed: Vec<_> = saved.into_iter().map(|(path, _)| path).collect();
            claimed.sort();
            assert_eq!(claimed, names, "fsync={fsync}");
            assert_eq!(walkdir(root), names, "fsync={fsync}");
        }
    }

    // LD-13: A claim reports whether it created its bucket, which is what
    // tells an atomic save to sync the directories above a new one.
    #[test]
    fn test_claim_reports_whether_it_created_the_bucket() {
        let dir = tempdir().unwrap();

        let first = placement(dir.path()).claim().unwrap();
        assert!(first.created_bucket, "the bucket did not exist");

        let second = placement(dir.path()).claim().unwrap();
        assert!(!second.created_bucket, "the bucket was already there");
    }

    // LD-14: A bucket removed from under a claim, between its creation and
    // the create of the file in it, is created again and the claim succeeds.
    #[test]
    fn test_claim_recreates_a_bucket_removed_under_it() {
        let dir = tempdir().unwrap();
        let (placement, creations) = placement_losing_its_bucket(dir.path(), 1);

        let claim = placement.claim().unwrap();

        assert_eq!(claim.path, bucket(dir.path()).join("1234"));
        assert!(claim.created_bucket);
        assert_eq!(creations.load(Ordering::Relaxed), 2);
    }

    // LD-15: A bucket that is gone every time the claim creates it fails the
    // save with the filesystem's `NotFound`, after `BUCKET_CREATIONS`
    // creations. The bucket is removed exactly that many times, so a claim
    // with no limit would succeed on the next creation and fail this test,
    // not run without end.
    #[test]
    fn test_claim_gives_up_on_a_bucket_that_keeps_disappearing() {
        let dir = tempdir().unwrap();
        let (placement, creations) = placement_losing_its_bucket(dir.path(), BUCKET_CREATIONS);

        let error = placement
            .claim()
            .err()
            .expect("the bucket is missing after every creation the claim may make");

        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(creations.load(Ordering::Relaxed), BUCKET_CREATIONS);
        assert_eq!(
            walkdir(dir.path()),
            Vec::<PathBuf>::new(),
            "no file was created"
        );
    }

    // Puts a directory under a name just claimed, so the rename that would
    // publish a bundle there fails: a file cannot replace a directory.
    fn put_a_directory_under_the_name(path: &Path) {
        remove_file(path).unwrap();
        create_dir_all(path).unwrap();
    }

    // LD-12: An atomic save that fails after writing its temporary file
    // removes it. The rename fails, the bundle already written and synced.
    #[test]
    fn test_failed_rename_removes_the_temporary_file() {
        let dir = tempdir().unwrap();
        let placement = Placement {
            after_claim: Some(put_a_directory_under_the_name),
            ..placement(dir.path())
        };

        let error = save_in_mode(true, &placement, b"bundle")
            .expect_err("a file cannot be renamed over a directory");

        // The rename's refusal differs by platform.
        let refusal = cfg_select! {
            windows => ErrorKind::PermissionDenied,
            _ => ErrorKind::IsADirectory,
        };
        assert_eq!(error.kind(), refusal);
        assert_eq!(
            walkdir(dir.path()),
            Vec::<PathBuf>::new(),
            "the temporary file is removed"
        );
    }

    // Puts another save's bundle under a name just claimed, as if the claimed
    // file had been removed and the freed name claimed again.
    fn give_name_to_another_save(path: &Path) {
        remove_file(path).unwrap();
        write(path, b"another save's bundle").unwrap();
    }

    // LD-11: An unsynced save writes only through the handle that claimed
    // its name, and never opens the name again. Another file takes the name
    // between the claim and the write: the save's bytes follow its own
    // handle, and the other file is untouched. A second, truncating open
    // would have replaced the other file's contents.
    #[tokio::test]
    async fn test_unsynced_save_never_reopens_its_name() {
        let (dir, store) = new_store(false);
        let placement = Placement {
            after_claim: Some(give_name_to_another_save),
            ..placement(dir.path())
        };

        let name = store
            .save_at(placement, Bytes::from_static(b"bundle"))
            .await
            .unwrap();

        assert_eq!(
            read(dir.path().join(&*name)).unwrap(),
            b"another save's bundle"
        );
    }
}
