#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::{
    io::Write,
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
    /// platform cache directory, and fsync enabled (each save writes to a
    /// `.tmp` file with `O_SYNC` / `FILE_FLAG_WRITE_THROUGH`, syncs data,
    /// renames to the final name, and syncs the parent directory;
    /// `Some(false)` uses plain `tokio::fs::write` instead).
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

#[cfg_attr(feature = "instrument", instrument(skip_all))]
fn random_file_path(root: &Path) -> Result<PathBuf, std::io::Error> {
    let mut rng = rand::rng();

    // Random subdirectory
    let dir1 = format!("{:02x}", rng.random::<u8>());
    let dir2 = format!("{:02x}", rng.random::<u8>());
    let dir_path = root.join(dir1).join(dir2);

    // Ensure directory exists
    std::fs::create_dir_all(&dir_path)?;

    let mut file_id = rng.random::<u32>() as u64;

    loop {
        // Add a random filename
        let file_path = dir_path.join(format!("{:x}", file_id));

        // Stop races between threads by creating a 0-length file
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file_path)
        {
            Ok(_) => return Ok(file_path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                file_id = file_id.wrapping_add(1);
            }
            Err(e) => return Err(e),
        }
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
        let storage_name = if self.fsync {
            let root = self.store_root.clone();
            tokio::task::spawn_blocking(move || {
                // Create random filename
                let mut storage_name = random_file_path(&root)?;

                /*
                create a new temp file (alongside the original)
                write data to the temp file
                fsync() the temp file
                rename the temp file to the original name
                fsync() the containing directory
                */

                // Use a temporary extension
                storage_name.set_extension("tmp");

                // Open the file as direct as possible
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);

                #[cfg(unix)]
                options.custom_flags(libc::O_SYNC);

                #[cfg(windows)]
                options.custom_flags(winapi::um::winbase::FILE_FLAG_WRITE_THROUGH);

                let mut file = options.open(&storage_name)?;

                // Write all data to file
                file.write_all(&data).inspect_err(|e| {
                    error!("Failed to write bundle data: {e}");
                    _ = std::fs::remove_file(&storage_name);
                })?;

                // Sync the data (we sync the directory after the rename)
                file.sync_data().inspect_err(|e| {
                    error!("Failed to sync bundle file data: {e}");
                    _ = std::fs::remove_file(&storage_name);
                })?;

                // Rename the file
                let old_path = storage_name.clone();
                storage_name.set_extension("");
                std::fs::rename(&old_path, &storage_name).inspect_err(|e| {
                    error!("Failed to rename temporary bundle data file to final name: {e}");
                    _ = std::fs::remove_file(&old_path);
                })?;

                // And now sync the parent directory, i.e. metadata
                if let Some(parent_dir) = storage_name.parent()
                    && let Err(e) = std::fs::File::open(parent_dir).and_then(|f| f.sync_all())
                {
                    warn!("Failed to sync parent directory: {e}");
                }

                storage::Result::Ok(storage_name)
            })
            .await
            .trace_expect("Failed to spawn write_atomic thread")?
        } else {
            let storage_name = random_file_path(&self.store_root)?;

            // Just use tokio write and hope for the best
            tokio::fs::write(&storage_name, &data)
                .await
                .inspect_err(|e| {
                    error!("Failed to write bundle data: {e}");
                    _ = std::fs::remove_file(&storage_name);
                })?;

            storage_name
        };

        Ok(storage_name
            .strip_prefix(&self.store_root)?
            .to_string_lossy()
            .into())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, data)))]
    async fn replace(&self, storage_name: &str, data: Bytes) -> storage::Result<()> {
        let final_path = self.store_root.join(PathBuf::from_str(storage_name)?);
        let tmp_path = final_path.with_extension("tmp");

        if self.fsync {
            let final_path = final_path.clone();
            let tmp_path = tmp_path.clone();
            tokio::task::spawn_blocking(move || {
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create(true).truncate(true);

                #[cfg(unix)]
                options.custom_flags(libc::O_SYNC);

                #[cfg(windows)]
                options.custom_flags(winapi::um::winbase::FILE_FLAG_WRITE_THROUGH);

                let mut file = options.open(&tmp_path)?;
                file.write_all(&data).inspect_err(|e| {
                    error!("Failed to write bundle data: {e}");
                    _ = std::fs::remove_file(&tmp_path);
                })?;
                file.sync_data().inspect_err(|e| {
                    error!("Failed to sync bundle file data: {e}");
                    _ = std::fs::remove_file(&tmp_path);
                })?;
                std::fs::rename(&tmp_path, &final_path).inspect_err(|e| {
                    error!("Failed to rename temporary bundle data file: {e}");
                    _ = std::fs::remove_file(&tmp_path);
                })?;
                if let Some(parent_dir) = final_path.parent()
                    && let Err(e) = std::fs::File::open(parent_dir).and_then(|f| f.sync_all())
                {
                    warn!("Failed to sync parent directory: {e}");
                }
                storage::Result::Ok(())
            })
            .await
            .trace_expect("Failed to spawn replace thread")?;
            Ok(())
        } else {
            tokio::fs::write(&tmp_path, &data).await.inspect_err(|e| {
                error!("Failed to write bundle data: {e}");
                _ = std::fs::remove_file(&tmp_path);
            })?;
            tokio::fs::rename(&tmp_path, &final_path)
                .await
                .inspect_err(|e| {
                    error!("Failed to rename temporary bundle data file: {e}");
                    _ = std::fs::remove_file(&tmp_path);
                })?;
            Ok(())
        }
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
