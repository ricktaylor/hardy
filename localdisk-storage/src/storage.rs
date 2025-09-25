use super::*;
use hardy_bpa::{Bytes, async_trait, storage, storage::BundleStorage};
use rand::prelude::*;
use std::{io::Write, path::PathBuf, str::FromStr, sync::Arc, time::SystemTime};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

pub struct Storage {
    store_root: PathBuf,
}

impl Storage {
    pub fn new(config: &Config, _upgrade: bool) -> Self {
        Self {
            store_root: config.store_dir.clone(),
        }
    }
}

#[cfg_attr(feature = "tracing", instrument(skip_all))]
fn random_file_path(root: &PathBuf) -> Result<PathBuf, std::io::Error> {
    let mut rng = rand::rng();
    loop {
        // Random subdirectory
        let mut file_path = [
            root,
            &PathBuf::from(format!("{:02x}", rng.random::<u16>() % 256)),
            &PathBuf::from(format!("{:02x}", rng.random::<u16>() % 256)),
        ]
        .iter()
        .collect::<PathBuf>();

        // Ensure directory exists
        std::fs::create_dir_all(&file_path)?;

        // Add a random filename
        file_path.push(PathBuf::from(format!("{:x}", rng.random::<u32>())));

        // Stop races between threads by creating a 0-length file
        if let Err(e) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file_path)
        {
            if let std::io::ErrorKind::AlreadyExists = e.kind() {
                continue;
            }
            return Err(e);
        } else {
            return Ok(file_path);
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
                    // Drop anything .tmp
                    if let Some(extension) = entry.path().extension()
                        && extension == "tmp"
                    {
                        std::fs::remove_file(entry.path())
                            .trace_expect("Failed to remove tmp file");
                        continue;
                    }

                    // There is a small race when restarting, whereby bundles expire during walk_dirs,
                    // So it is perfectly valid for the file to no longer exist

                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };

                    // Drop 0-length files
                    if metadata.len() == 0 {
                        if let Err(e) = std::fs::remove_file(entry.path())
                            && !matches!(e.kind(), std::io::ErrorKind::NotFound)
                        {
                            tracing::error!("Failed to remove placeholder file");
                            panic!("Failed to remove placeholder file");
                        }
                        continue;
                    }

                    let Ok(received_at) = metadata.created() else {
                        continue;
                    };

                    // Ignore anything created after we began our walk
                    if &received_at > before {
                        continue;
                    }

                    if tx
                        .send((
                            entry
                                .path()
                                .strip_prefix(root)
                                .unwrap()
                                .to_string_lossy()
                                .into(),
                            time::OffsetDateTime::from(received_at),
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

        cfg_if::cfg_if! {
            if #[cfg(feature = "mmap")] {
                let file = match tokio::fs::File::open(storage_name).await {
                    Err(e) => {
                        if let std::io::ErrorKind::NotFound = e.kind() {
                            return Ok(None)
                        } else {
                            return Err(e.into())
                        }
                    }
                    Ok(file) => file,
                };
                let data = unsafe { memmap2::Mmap::map(&file) };
                Ok(Some(Bytes::from_owner(data?)))
            } else {
                match tokio::fs::read(storage_name).await {
                    Err(e) => {
                        if let std::io::ErrorKind::NotFound = e.kind() {
                            Ok(None)
                        } else {
                            Err(e.into())
                        }
                    }
                    Ok(data) => Ok(Bytes::from_owner(data))
                }
            }
        }
    }

    #[cfg_attr(feature = "tracing", instrument(skip_all))]
    async fn save(&self, data: Bytes) -> storage::Result<Arc<str>> {
        let root = self.store_root.clone();

        // Spawn a thread to try to maintain linearity
        let storage_name = tokio::task::spawn_blocking(move || {
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
            cfg_if::cfg_if! {
                if #[cfg(unix)] {
                    options.custom_flags(libc::O_SYNC);
                } else if #[cfg(windows)] {
                    options.custom_flags(winapi::um::winbase::FILE_FLAG_WRITE_THROUGH);
                }
            }
            let mut file = options.open(&storage_name)?;

            if let Err(e) = {
                // Write all data to file
                file.write_all(&data)?;

                // Sync everything
                file.sync_all()
            } {
                _ = std::fs::remove_file(&storage_name);
                return Err(e);
            }

            // Rename the file
            let old_path = storage_name.clone();
            storage_name.set_extension("");
            if let Err(e) = std::fs::rename(&old_path, &storage_name) {
                _ = std::fs::remove_file(&old_path);
                return Err(e);
            }

            // No idea how to fsync the directory in portable Rust!

            Ok(storage_name)
        })
        .await
        .trace_expect("Failed to spawn write_atomic thread")?;

        Ok(storage_name
            .strip_prefix(&self.store_root)?
            .to_string_lossy()
            .into())
    }

    #[cfg_attr(feature = "tracing", instrument(skip(self)))]
    async fn delete(&self, storage_name: &str) -> storage::Result<()> {
        match tokio::fs::remove_file(&self.store_root.join(PathBuf::from_str(storage_name)?)).await
        {
            Ok(_) => Ok(()),
            Err(e) => {
                if let std::io::ErrorKind::NotFound = e.kind() {
                    warn!("Failed to remove {storage_name}");
                    Ok(())
                } else {
                    Err(e.into())
                }
            }
        }
    }
}
