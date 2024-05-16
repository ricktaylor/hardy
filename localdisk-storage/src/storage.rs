use super::*;
use hardy_bpa_core::{async_trait, storage, storage::BundleStorage, storage::DataRef};
use rand::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};
use trace_err::*;
use tracing::*;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

pub struct Storage {
    store_root: PathBuf,
    reserved_paths: Mutex<HashSet<PathBuf>>,
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
                                Ok(Path::new("/var/spool").join(built_info::PKG_NAME))
                            } else if #[cfg(windows)] {
                                Ok(std::env::current_exe().join(built_info::PKG_NAME))
                            } else {
                                compile_error!("No idea how to determine default local store directory for target platform")
                            }
                        }
                    },
                    |project_dirs| {
                        Ok(project_dirs.cache_dir().into())
                        // Lin: /home/alice/.cache/barapp
                        // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\cache
                        // Mac: /Users/Alice/Library/Caches/com.Foo-Corp.Bar-App
                    },
                )
            },
            |v| {
                v.clone().into_string().map(|s| s.into())
            },
        ).trace_expect("Failed to determine bundle store directory");

        info!("Using bundle store directory: {}", store_root.display());

        // Ensure directory exists
        std::fs::create_dir_all(&store_root).trace_expect(&format!(
            "Failed to create bundle store directory {}",
            store_root.display()
        ));

        Arc::new(Storage {
            store_root,
            reserved_paths: Mutex::new(HashSet::new()),
        })
    }

    fn random_file_path(&self) -> Result<PathBuf, std::io::Error> {
        // Compose a subdirectory that doesn't break filesystems
        let mut rng = rand::thread_rng();
        let sub_dir = [
            format!("{:x}", rng.gen::<u16>() % 4096),
            format!("{:x}", rng.gen::<u16>() % 4096),
            format!("{:x}", rng.gen::<u16>() % 4096),
        ]
        .iter()
        .collect::<PathBuf>();

        // Random filename
        loop {
            let file_path = [
                &self.store_root,
                &sub_dir,
                &PathBuf::from(format!("{:x}", rng.gen::<u64>() % 4096)),
            ]
            .iter()
            .collect::<PathBuf>();

            // Stop races between threads
            if self
                .reserved_paths
                .lock()
                .trace_expect("Failed to lock reserved_paths mutex")
                .insert(file_path.clone())
            {
                // Check if a file with that name doesn't exist
                match std::fs::metadata(&file_path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(file_path),
                    r => {
                        // Remove the reserved_paths entry
                        self.reserved_paths
                            .lock()
                            .trace_expect("Failed to lock reserved_paths mutex")
                            .remove(&file_path);
                        r?;
                    }
                }
            }
        }
    }

    #[allow(clippy::type_complexity)]
    #[instrument(level = "debug", skip(self, f))]
    fn walk_dirs(
        &self,
        dir: &PathBuf,
        f: &mut dyn FnMut(&str, &[u8], Option<time::OffsetDateTime>) -> storage::Result<bool>,
    ) -> storage::Result<bool> {
        for entry in std::fs::read_dir(dir)?.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    if !self.walk_dirs(&entry.path(), f)? {
                        return Ok(false);
                    }
                } else if file_type.is_file() {
                    if let Some(extension) = entry.path().extension() {
                        // Drop anything .tmp
                        if extension == "tmp" {
                            std::fs::remove_file(entry.path())?;
                            continue;
                        }
                    }

                    // Report orphan
                    let storage_path = entry.path();
                    let storage_name = storage_path
                        .strip_prefix(&self.store_root)?
                        .to_string_lossy();
                    let received_at = entry
                        .metadata()
                        .and_then(|m| m.created())
                        .map(time::OffsetDateTime::from)
                        .ok();

                    let hash = self.hash(self.sync_load(&storage_name)?.as_ref().as_ref());
                    if !f(&storage_name, &hash, received_at)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }

    fn sync_load(&self, storage_name: &str) -> storage::Result<DataRef> {
        let file_path = self.store_root.join(PathBuf::from_str(storage_name)?);

        cfg_if::cfg_if! {
            if #[cfg(feature = "mmap")] {
                let file = std::fs::File::open(file_path)?;
                let data = unsafe { memmap2::Mmap::map(&file)? };
                Ok(Arc::new(data))
            } else {
                let mut v = Vec::new();
                std::fs::File::open(file_path)?.read_to_end(&mut v)?;
                Ok(Arc::new(v))
            }
        }
    }
}

#[async_trait]
impl BundleStorage for Storage {
    #[instrument(skip(self, f))]
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(&str, &[u8], Option<time::OffsetDateTime>) -> storage::Result<bool>,
    ) -> storage::Result<()> {
        self.walk_dirs(&self.store_root, f).map(|_| ())
    }

    #[instrument(skip(self))]
    async fn load(&self, storage_name: &str) -> storage::Result<DataRef> {
        self.sync_load(storage_name)
    }

    #[instrument(skip_all)]
    async fn store(&self, data: Vec<u8>) -> storage::Result<String> {
        /*
        create a new temp file (on the same file system!)
        write data to the temp file
        fsync() the temp file
        rename the temp file to the appropriate name
        fsync() the containing directory
        */

        // Create random filename
        let file_path = self.random_file_path()?;
        let file_path_cloned = file_path.clone();

        // Perform blocking I/O on dedicated worker task
        let result = tokio::task::spawn_blocking(move || write(file_path_cloned, data)).await;

        // Always remove tmps entry
        self.reserved_paths
            .lock()
            .trace_expect("Failed to lock reserved_paths mutex")
            .remove(&file_path);

        // Check result errors
        result??;

        Ok(file_path
            .strip_prefix(&self.store_root)?
            .to_string_lossy()
            .to_string())
    }

    #[instrument(skip(self))]
    async fn remove(&self, storage_name: &str) -> storage::Result<()> {
        let file_path = self.store_root.join(PathBuf::from_str(storage_name)?);
        tokio::fs::remove_file(&file_path)
            .await
            .map_err(|e| e.into())
    }

    #[instrument(skip(self, data))]
    async fn replace(&self, storage_name: &str, data: Vec<u8>) -> storage::Result<()> {
        /*
        create a new temp file (alongside the original)
        write data to the temp file
        fsync() the temp file
        rename the temp file to the original name
        fsync() the containing directory
        */

        // Create random filename
        let file_path = PathBuf::from_str(storage_name)?;
        let file_path_cloned = file_path.clone();

        // Perform blocking I/O on dedicated worker task
        tokio::task::spawn_blocking(move || write(file_path_cloned, data))
            .await?
            .map_err(|e| e.into())
    }
}

#[instrument(skip(data))]
fn write(mut file_path: PathBuf, data: Vec<u8>) -> std::io::Result<()> {
    // Ensure directory exists
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Use a temporary extension
    file_path.set_extension("tmp");

    // Open the file as direct as possible
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            options.custom_flags(libc::O_SYNC | libc::O_DIRECT);
        } else if #[cfg(windows)] {
            options.custom_flags(winapi::FILE_FLAG_WRITE_THROUGH);
        }
    }
    let mut file = options.open(&file_path)?;

    // Write all data to file
    if let Err(e) = file.write_all(&data) {
        _ = std::fs::remove_file(&file_path);
        return Err(e);
    }

    // Sync everything
    if let Err(e) = file.sync_all() {
        _ = std::fs::remove_file(&file_path);
        return Err(e);
    }

    // Rename the file
    let old_path = file_path.clone();
    file_path.set_extension("");
    if let Err(e) = std::fs::rename(&old_path, &file_path) {
        _ = std::fs::remove_file(&old_path);
        return Err(e);
    }

    // No idea how to fsync the directory in portable Rust!

    Ok(())
}
