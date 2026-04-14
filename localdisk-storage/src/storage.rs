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
    #[cfg_attr(feature = "instrument", instrument(skip_all))]
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

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
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
    use super::*;
    use hardy_bpa::storage::BundleStorage;

    fn make_config(dir: &std::path::Path, fsync: bool) -> crate::Config {
        crate::Config {
            store_dir: dir.to_path_buf(),
            fsync,
        }
    }

    /// LD-01: Files are created under the configured store_dir.
    #[tokio::test]
    async fn test_configuration_custom_store_dir() {
        let dir = tempfile::tempdir().unwrap();
        let store = Storage::new(&make_config(dir.path(), false), false);

        let name = store.save(Bytes::from_static(b"hello")).await.unwrap();
        let full_path = dir.path().join(&*name);
        assert!(
            full_path.exists(),
            "bundle file should exist under configured store_dir"
        );
    }

    /// LD-02: Recovery cleans up .tmp files, zero-byte placeholders, and empty dirs.
    #[tokio::test]
    async fn test_recovery_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let store = Storage::new(&make_config(dir.path(), false), false);

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
        let (tx, rx) = flume::unbounded();
        store.recover(tx).await.unwrap();

        // Collect recovered entries
        let recovered: Vec<_> = rx.drain().collect();

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

    /// LD-03: Files are distributed in a two-level xx/yy/ directory structure.
    #[tokio::test]
    async fn test_filesystem_structure() {
        let dir = tempfile::tempdir().unwrap();
        let store = Storage::new(&make_config(dir.path(), false), false);

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

    /// LD-04: With fsync=true, save writes to .tmp then renames (no .tmp left behind).
    #[tokio::test]
    async fn test_atomic_save_no_tmp_residue() {
        let dir = tempfile::tempdir().unwrap();
        let store = Storage::new(&make_config(dir.path(), true), false);

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

    /// LD-05: Save on a read-only directory returns an error, not a panic.
    #[tokio::test]
    async fn test_save_to_readonly_dir_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Storage::new(&make_config(dir.path(), false), false);

        // Make the store directory read-only
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(dir.path(), perms.clone()).unwrap();

        // save() should return an error, not panic
        let result = store.save(Bytes::from_static(b"fail")).await;
        assert!(result.is_err(), "save to read-only dir should return Err");

        // Restore permissions so tempdir cleanup succeeds
        perms.set_readonly(false);
        std::fs::set_permissions(dir.path(), perms).unwrap();
    }

    /// Recursively collect all file paths under a directory.
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
