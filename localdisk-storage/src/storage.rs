use super::*;
use hardy_bpa_api::{async_trait, storage, storage::BundleStorage, storage::DataRef};
use rand::prelude::*;
use sha2::Digest;
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use trace_err::*;
use tracing::*;

pub struct Storage {
    store_root: PathBuf,
}

impl Storage {
    #[instrument(skip(config))]
    pub fn init(config: &HashMap<String, config::Value>) -> Arc<dyn BundleStorage> {
        let store_root = config.get("store_dir").map_or_else(
            || {
                directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME).map_or_else(
                    || {
                        cfg_if::cfg_if! {
                            if #[cfg(unix)] {
                                Path::new("/var/spool").join(built_info::PKG_NAME)
                            } else if #[cfg(windows)] {
                                std::env::current_exe().join(built_info::PKG_NAME)
                            } else {
                                compile_error!("No idea how to determine default local store directory for target platform")
                            }
                        }
                    },
                    |project_dirs| {
                        project_dirs.cache_dir().into()
                        // Lin: /home/alice/.cache/barapp
                        // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\cache
                        // Mac: /Users/Alice/Library/Caches/com.Foo-Corp.Bar-App
                    },
                )
            },
            |v| {
                v.clone().into_string().trace_expect("Invalid 'store_dir' value in configuration").into()
            },
        );

        info!("Using bundle store directory: {}", store_root.display());

        // Ensure directory exists
        std::fs::create_dir_all(&store_root).trace_expect(&format!(
            "Failed to create bundle store directory {}",
            store_root.display()
        ));

        Arc::new(Storage { store_root })
    }
}

fn hash(data: &[u8]) -> Arc<[u8]> {
    Arc::from(sha2::Sha256::digest(data).as_slice())
}

fn random_file_path(root: &PathBuf) -> Result<PathBuf, std::io::Error> {
    let mut rng = rand::thread_rng();
    loop {
        // Random subdirectory
        let mut file_path = [
            root,
            &PathBuf::from(format!("{:x}", rng.gen::<u16>() % 4096)),
            &PathBuf::from(format!("{:x}", rng.gen::<u16>() % 4096)),
            &PathBuf::from(format!("{:x}", rng.gen::<u16>() % 4096)),
        ]
        .iter()
        .collect::<PathBuf>();

        // Ensure directory exists
        std::fs::create_dir_all(&file_path)?;

        // Add a random filename
        file_path.push(PathBuf::from(format!("{:x}", rng.gen::<u16>() % 4096)));

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

fn walk_dirs(
    root: &PathBuf,
    dir: PathBuf,
    tx: tokio::sync::mpsc::Sender<storage::ListResponse>,
) -> usize {
    let mut count: usize = 0;
    if let Ok(dir) = std::fs::read_dir(dir.clone()) {
        for entry in dir.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    count += walk_dirs(root, entry.path(), tx.clone());
                } else if file_type.is_file() {
                    // Drop anything .tmp
                    if let Some(extension) = entry.path().extension() {
                        if extension == "tmp" {
                            std::fs::remove_file(entry.path())
                                .trace_expect("Failed to remove tmp file");
                            continue;
                        }
                    }

                    // Report a bundle
                    let storage_path = entry.path();
                    cfg_if::cfg_if! {
                        if #[cfg(feature = "mmap")] {
                            let file = std::fs::File::open(&storage_path).trace_expect("Failed to open file");
                            let data = unsafe { memmap2::Mmap::map(&file) }.map(Arc::new).trace_expect("Failed to memory map file");
                        } else {
                            let data = std::fs::read(&storage_path).map(Arc::new).trace_expect("Failed to read file content");
                        }
                    }

                    // Drop 0-length files
                    if data.is_empty() {
                        std::fs::remove_file(entry.path())
                            .trace_expect("Failed to remove placeholder file");
                        continue;
                    }

                    // We haver something useful
                    count += 1;

                    let hash = hash(data.as_ref().as_ref());
                    let received_at = entry
                        .metadata()
                        .and_then(|m| m.created())
                        .map(time::OffsetDateTime::from)
                        .ok();

                    if tx
                        .blocking_send((
                            Arc::from(storage_path.strip_prefix(root).unwrap().to_string_lossy()),
                            hash,
                            data,
                            received_at,
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }

    if count == 0 && std::fs::remove_dir(&dir).is_err() {
        count = 1;
    }
    count
}

#[async_trait]
impl BundleStorage for Storage {
    fn hash(&self, data: &[u8]) -> Arc<[u8]> {
        hash(data)
    }

    #[instrument(skip_all)]
    async fn list(
        &self,
        tx: tokio::sync::mpsc::Sender<storage::ListResponse>,
    ) -> storage::Result<()> {
        let root = self.store_root.clone();

        // Spawn a thread to walk the directory
        tokio::task::spawn_blocking(move || walk_dirs(&root.clone(), root, tx))
            .await
            .trace_expect("Failed to spawn walk_dirs thread");
        Ok(())
    }

    #[instrument(skip(self))]
    async fn load(&self, storage_name: &str) -> storage::Result<DataRef> {
        let storage_name = self.store_root.join(PathBuf::from_str(storage_name)?);
        cfg_if::cfg_if! {
            if #[cfg(feature = "mmap")] {
                let file = tokio::fs::File::open(storage_name).await?;
                let data = unsafe { memmap2::Mmap::map(&file) };
                Ok(Arc::new(data?))
            } else {
                Ok(Arc::new(tokio::fs::read(storage_name).await?))
            }
        }
    }

    async fn store(&self, data: Arc<[u8]>) -> storage::Result<(Arc<str>, Arc<[u8]>)> {
        let hash = hash(&data);
        let root = self.store_root.clone();

        // Spawn a thread to try to maintain linearity
        let storage_name = tokio::task::spawn_blocking(move || {
            // Create random filename
            let storage_name = random_file_path(&root)?;

            // Write to disk
            write_atomic(storage_name.clone(), &data).map(|_| storage_name)
        })
        .await
        .trace_expect("Failed to spawn write_atomic thread")?;

        Ok((
            Arc::from(
                storage_name
                    .strip_prefix(&self.store_root)?
                    .to_string_lossy(),
            ),
            hash,
        ))
    }

    #[instrument(skip(self))]
    async fn remove(&self, storage_name: &str) -> storage::Result<()> {
        tokio::fs::remove_file(&self.store_root.join(PathBuf::from_str(storage_name)?))
            .await
            .map_err(Into::into)
    }

    #[instrument(skip(self, data))]
    async fn replace(&self, storage_name: &str, data: Box<[u8]>) -> storage::Result<()> {
        let storage_name = PathBuf::from_str(storage_name)?;

        // Spawn a thread to try to maintain linearity
        tokio::task::spawn_blocking(move || write_atomic(storage_name, &data))
            .await
            .trace_expect("Failed to spawn write_atomic thread")
    }
}

#[instrument(skip(data))]
fn write_atomic(mut file_path: PathBuf, data: &[u8]) -> storage::Result<()> {
    /*
    create a new temp file (alongside the original)
    write data to the temp file
    fsync() the temp file
    rename the temp file to the original name
    fsync() the containing directory
    */

    // Use a temporary extension
    file_path.set_extension("tmp");

    // Open the file as direct as possible
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            options.custom_flags(libc::O_SYNC);
        } else if #[cfg(windows)] {
            options.custom_flags(winapi::FILE_FLAG_WRITE_THROUGH);
        }
    }
    let mut file = options.open(&file_path)?;

    // Write all data to file
    if let Err(e) = file.write_all(data) {
        _ = std::fs::remove_file(&file_path);
        return Err(e.into());
    }

    // Sync everything
    if let Err(e) = file.sync_all() {
        _ = std::fs::remove_file(&file_path);
        return Err(e.into());
    }

    // Rename the file
    let old_path = file_path.clone();
    file_path.set_extension("");
    if let Err(e) = std::fs::rename(&old_path, &file_path) {
        _ = std::fs::remove_file(&old_path);
        return Err(e.into());
    }

    // No idea how to fsync the directory in portable Rust!

    Ok(())
}
