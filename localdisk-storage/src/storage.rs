use super::*;
use hardy_bpa::{Bytes, async_trait, storage, storage::BundleStorage};
use rand::prelude::*;
use std::{
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::SystemTime,
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

pub struct Storage {
    store_root: PathBuf,
    fsync: bool,
}

impl Storage {
    pub fn new(config: &Config, _upgrade: bool) -> Self {
        Self {
            store_root: config.store_dir.clone(),
            fsync: config.fsync,
        }
    }
}

#[cfg_attr(feature = "tracing", instrument(skip_all))]
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

#[cfg_attr(feature = "tracing", instrument(skip(tx)))]
fn walk_dirs(
    before: &SystemTime,
    root: &PathBuf,
    dir: PathBuf,
    tx: &storage::Sender<storage::RecoveryResponse>,
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
impl BundleStorage for Storage {
    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn recover(&self, tx: storage::Sender<storage::RecoveryResponse>) -> storage::Result<()> {
        let before = SystemTime::now();
        let mut dirs = vec![self.store_root.clone()];

        let parallelism = std::thread::available_parallelism()
            .map(Into::into)
            .unwrap_or(1);
        let mut task_set = tokio::task::JoinSet::new();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(parallelism));

        // Loop through the directories
        while !dirs.is_empty() && !tx.is_disconnected() {
            // Take a chunk off the back, to ensure depth first walk
            let subdirs = dirs.split_off(dirs.len() - dirs.len().min(32));

            loop {
                tokio::select! {
                    // Throttle the number of threads
                    permit = semaphore.clone().acquire_owned() => {
                        let permit = permit.trace_expect("Failed to acquire permit");
                        let root = self.store_root.clone();
                        let tx = tx.clone();
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

            while dirs.is_empty() || tx.is_disconnected() {
                // Accumulate results
                let Some(r) = task_set.join_next().await else {
                    break;
                };
                dirs.extend(r.trace_expect("Task terminated unexpectedly"));
            }
        }
        Ok(())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn load(&self, storage_name: &str) -> storage::Result<Option<Bytes>> {
        let storage_name = self.store_root.join(PathBuf::from_str(storage_name)?);

        #[cfg(feature = "mmap")]
        {
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

        #[cfg(not(feature = "mmap"))]
        tokio::fs::read(storage_name)
            .await
            .map(|data| Some(Bytes::from_owner(data)))
            .or_else(|e| match e.kind() {
                std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(e.into()),
            })
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
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
                    && let Ok(dir_handle) = std::fs::File::open(parent_dir)
                    && let Err(e) = dir_handle.sync_all()
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

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
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
    // use super::*;

    // #[tokio::test]
    // async fn test_atomic_save_ld_01() {
    //     // TODO: LD-01 Atomic Save (fsync)
    //     // Verifies the "save-to-temp, then rename" logic when `fsync` is enabled.
    //     // Tests should simulate a crash (panic) after the temp file is written but before the rename,
    //     // ensuring no partial bundle file is left with the final name.
    //     todo!("Implement test_atomic_save_ld_01");
    // }

    // #[tokio::test]
    // async fn test_recovery_logic_ld_02() {
    //     // TODO: LD-02 Recovery Logic
    //     // Verifies the `recover()` function correctly handles a dirty storage directory.
    //     // The test should set up a directory containing:
    //     // 1. Valid bundle files.
    //     // 2. Leftover `.tmp` files.
    //     // 3. Zero-byte placeholder files.
    //     // 4. Empty subdirectories.
    //     // The test must confirm that only valid bundles are recovered and that temporary files
    //     // and empty directories are cleaned up.
    //     todo!("Implement test_recovery_logic_ld_02");
    // }

    // #[tokio::test]
    // async fn test_filesystem_structure_ld_03() {
    //     // TODO: LD-03 Filesystem Structure
    //     // Verifies that the `xx/yy/` two-level directory structure is created correctly.
    //     // It should also stress-test the filename collision logic by forcing many saves
    //     // into the same subdirectory bucket to ensure it resolves collisions without error.
    //     todo!("Implement test_filesystem_structure_ld_03");
    // }

    // // TODO: LD-04 `mmap` Feature Flag
    // // All test suites (Generic and Specific) must be run with and without the `mmap` feature flag enabled.

    // #[tokio::test]
    // async fn test_persistence_ld_05() {
    //     // TODO: LD-05 Persistence
    //     // Verifies that saved data survives the `Storage` object being dropped and recreated,
    //     // pointing to the same `store_dir`. This confirms that file paths and recovery work across restarts.
    //     todo!("Implement test_persistence_ld_05");
    // }
}
