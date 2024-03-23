use super::*;
use anyhow::anyhow;
use hardy_bpa_core::storage::{BundleStorage, MetadataStorage};
use rand::random;
use std::{
    collections::{HashMap, HashSet},
    fs::{create_dir_all, remove_file, OpenOptions},
    io::{self, Write},
    path::PathBuf,
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
    partials: Mutex<HashSet<PathBuf>>,
}

impl Storage {
    pub fn init(
        config: &HashMap<String, config::Value>,
    ) -> Result<std::sync::Arc<Self>, anyhow::Error> {
        let cache_dir: String = config.get("cache_dir").map_or_else(
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

        Ok(Arc::new(Storage {
            cache_root: PathBuf::from(&cache_dir),
            partials: Mutex::new(HashSet::new()),
        }))
    }

    fn random_file_path(&self) -> Result<PathBuf, std::io::Error> {
        // Compose a subdirectory
        let sub_dir = [
            &(random::<u16>() % 4096).to_string(),
            &(random::<u16>() % 4096).to_string(),
            &(random::<u16>() % 4096).to_string(),
        ]
        .iter()
        .collect::<PathBuf>();

        // Random filename
        loop {
            let file_path = [
                &self.cache_root,
                &sub_dir,
                &PathBuf::from(random::<u64>().to_string()),
            ]
            .iter()
            .collect::<PathBuf>();

            // Stop races between threads
            if self.partials.lock().unwrap().insert(file_path.clone()) {
                // Check if a file with that name doesn't exist
                match std::fs::metadata(&file_path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(file_path),
                    r => {
                        // Remove the partials entry
                        self.partials.lock().unwrap().remove(&file_path);
                        r?;
                    }
                }
            }
        }
    }
}

impl BundleStorage for Storage {
    async fn check<M, F>(
        &self,
        metadata: std::sync::Arc<M>,
        cancel_token: tokio_util::sync::CancellationToken,
        f: F,
    ) -> Result<(), anyhow::Error>
    where
        M: storage::MetadataStorage + Send + Sync,
        F: FnMut(&str, &[u8]) -> Result<bool, anyhow::Error> + Send,
    {
        todo!()
    }

    async fn store(&self, data: std::sync::Arc<Vec<u8>>) -> Result<String, anyhow::Error> {
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
            file_path_cloned.set_extension("partial");

            // Open the file as direct as possible
            let mut options = OpenOptions::new();
            options.write(true).create(true);
            if cfg!(windows) || cfg!(unix) {
                direct_flag(&mut options);
            }
            let mut file = options.open(&file_path_cloned)?;

            // Write all data to file
            if let Err(e) = file.write_all(data.as_ref()) {
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

        // Always remove partials entry
        self.partials.lock().unwrap().remove(&file_path);

        // Check result errors
        result??;

        Ok(file_path.to_string_lossy().to_string())
    }

    async fn remove(&self, _storage_name: &str) -> Result<bool, anyhow::Error> {
        todo!()
    }
}
