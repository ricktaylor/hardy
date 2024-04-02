use super::*;
use anyhow::anyhow;
use hardy_bpa_core::storage::BundleStorage;
use rand::random;
use std::{
    collections::{HashMap, HashSet},
    fs::{create_dir_all, remove_file, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

fn direct_flag(options: &mut OpenOptions) {
    #[cfg(unix)]
    options.custom_flags(libc::O_SYNC | libc::O_DIRECT);

    #[cfg(windows)]
    options.custom_flags(winapi::FILE_FLAG_WRITE_THROUGH);
}

pub struct Storage {
    cache_root: PathBuf,
    reserved_paths: Mutex<HashSet<PathBuf>>,
}

impl Storage {
    pub fn init(config: &HashMap<String, config::Value>) -> Result<Arc<Self>, anyhow::Error> {
        let cache_root: String = config.get("cache_dir").map_or_else(
            || {
                directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME).map_or_else(
                    || Err(anyhow!("Failed to resolve local cache directory")),
                    |project_dirs| {
                        Ok(project_dirs.cache_dir().to_string_lossy().to_string())
                        // Lin: /home/alice/.cache/barapp
                        // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\cache
                        // Mac: /Users/Alice/Library/Caches/com.Foo-Corp.Bar-App
                    },
                )
            },
            |v| {
                v.clone()
                    .into_string()
                    .map_err(|e| anyhow!("'cache_dir' is not a string value: {}!", e))
            },
        )?;

        // Ensure directory exists
        let cache_root = PathBuf::from(&cache_root);
        create_dir_all(&cache_root)?;

        Ok(Arc::new(Storage {
            cache_root,
            reserved_paths: Mutex::new(HashSet::new()),
        }))
    }

    fn random_file_path(&self) -> Result<PathBuf, std::io::Error> {
        // Compose a subdirectory that doesn't break filesystems
        let sub_dir = [
            format!("{:x}", random::<u16>() % 4096),
            format!("{:x}", random::<u16>() % 4096),
            format!("{:x}", random::<u16>() % 4096),
        ]
        .iter()
        .collect::<PathBuf>();

        // Random filename
        loop {
            let file_path = [
                &self.cache_root,
                &sub_dir,
                &PathBuf::from(format!("{:x}", random::<u64>() % 4096)),
            ]
            .iter()
            .collect::<PathBuf>();

            // Stop races between threads
            if self
                .reserved_paths
                .lock()
                .unwrap()
                .insert(file_path.clone())
            {
                // Check if a file with that name doesn't exist
                match std::fs::metadata(&file_path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(file_path),
                    r => {
                        // Remove the reserved_paths entry
                        self.reserved_paths.lock().unwrap().remove(&file_path);
                        r?;
                    }
                }
            }
        }
    }

    fn walk_dirs<F>(&self, dir: &PathBuf, f: &mut F) -> Result<bool, anyhow::Error>
    where
        F: FnMut(&str) -> Result<Option<bool>, anyhow::Error>,
    {
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

                    // Check corresponding bundle exists in the metadata storage
                    let storage_path = entry.path();
                    let storage_name = storage_path
                        .strip_prefix(&self.cache_root)?
                        .to_string_lossy();
                    match f(&storage_name)? {
                        Some(false) => {
                            // Remove from cache
                            std::fs::remove_file(storage_path)?;
                        }
                        None => {
                            return Ok(false);
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(true)
    }
}

impl BundleStorage for Storage {
    fn check_orphans<F>(&self, mut f: F) -> Result<(), anyhow::Error>
    where
        F: FnMut(&str) -> Result<Option<bool>, anyhow::Error>,
    {
        self.walk_dirs(&self.cache_root, &mut f).map(|_| ())
    }

    async fn load(&self, storage_name: &str) -> Result<Arc<Box<dyn AsRef<[u8]>>>, anyhow::Error> {
        let file_path = self.cache_root.join(PathBuf::from_str(storage_name)?);

        if cfg!(feature = "mmap") {
            let file = std::fs::File::open(file_path)?;
            let data = unsafe { memmap2::Mmap::map(&file)? };
            Ok(Arc::new(Box::new(data)))
        } else {
            let mut v = Vec::new();
            std::fs::File::open(file_path)?.read_to_end(&mut v)?;
            Ok(Arc::new(Box::new(v)))
        }
    }

    async fn store(&self, data: Arc<Vec<u8>>) -> Result<String, anyhow::Error> {
        /*
        create a new temp file (on the same file system!)
        write data to the temp file
        fsync() the temp file
        rename the temp file to the appropriate name
        fsync() the containing directory
        */

        // Create random filename
        let file_path = self.random_file_path()?;
        let mut file_path_cloned = file_path.clone();

        // Perform blocking I/O on dedicated worker task
        let result = tokio::task::spawn_blocking(move || {
            // Ensure directory exists
            create_dir_all(file_path_cloned.parent().unwrap())?;

            // Use a temporary extension
            file_path_cloned.set_extension("tmp");

            // Open the file as direct as possible
            let mut options = OpenOptions::new();
            options.write(true).create(true);
            if cfg!(windows) || cfg!(unix) {
                direct_flag(&mut options);
            }
            let mut file = options.open(&file_path_cloned)?;

            // Write all data to file
            if let Err(e) = file.write_all(&data) {
                _ = remove_file(&file_path_cloned);
                return Err(e);
            }

            // Sync everything
            if let Err(e) = file.sync_all() {
                _ = remove_file(&file_path_cloned);
                return Err(e);
            }

            // Rename the file
            let old_path = file_path_cloned.clone();
            file_path_cloned.set_extension("");
            if let Err(e) = std::fs::rename(&old_path, &file_path_cloned) {
                _ = remove_file(&old_path);
                return Err(e);
            }

            // No idea how to fsync the directory in portable Rust!

            Ok(())
        })
        .await;

        // Always remove tmps entry
        self.reserved_paths.lock().unwrap().remove(&file_path);

        // Check result errors
        result??;

        Ok(file_path
            .strip_prefix(&self.cache_root)?
            .to_string_lossy()
            .to_string())
    }

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error> {
        let file_path = self.cache_root.join(PathBuf::from_str(storage_name)?);
        match tokio::fs::remove_file(&file_path).await {
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    Ok(false)
                } else {
                    Err(e.into())
                }
            }
            Ok(_) => Ok(true),
        }
    }
}
