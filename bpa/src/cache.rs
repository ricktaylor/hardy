use super::*;
use rand::random;
use std::{
    collections::HashSet,
    fs::{create_dir_all, remove_file, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(unix)]
fn direct_flag(options: &mut OpenOptions) {
    options.custom_flags(libc::O_SYNC | libc::O_DIRECT);
}

#[cfg(windows)]
extern crate winapi;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

#[cfg(windows)]
fn direct_flag(options: &mut OpenOptions) {
    options.custom_flags(winapi::FILE_FLAG_WRITE_THROUGH);
}

#[derive(Debug, Clone)]
pub struct Cache {
    cache_root: PathBuf,
    partials: Arc<Mutex<HashSet<PathBuf>>>,
}

impl Cache {
    pub fn init(config: &settings::Config) -> Self {
        Self {
            cache_root: PathBuf::from(&config.cache_dir),
            partials: Arc::new(Mutex::new(HashSet::new())),
        }
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

    pub async fn store(&self, data: &Arc<Vec<u8>>) -> Result<Option<String>, anyhow::Error> {
        // Create random filename
        let file_path = self.random_file_path()?;

        // Start the write to disk
        let write_handle = write_bundle(file_path.clone(), data.clone());

        // Parse the bundle in parallel
        let bundle_result = bundle::parse(data);

        // Await the result of write_bundle
        let write_result = write_handle.await;

        // Always remove partials entry
        self.partials.lock().unwrap().remove(&file_path);

        // Check result of write_bundle
        write_result??;

        // Check result of bundle parse
        let bundle = match bundle_result {
            Ok(b) => b,
            Err(e) => {
                // Remove the cached file
                _ = tokio::fs::remove_file(&file_path).await;

                // Reply with forwarding failure - NOT an error
                return Ok(Some(format!("Bundle validation failed: {}", e.to_string())));
            }
        };

        // No failure
        Ok(None)
    }
}

fn write_bundle(
    mut file_path: PathBuf,
    data: Arc<Vec<u8>>,
) -> tokio::task::JoinHandle<Result<(), std::io::Error>> {
    /*
    create a new temp file (on the same file system!)
    write data to the temp file
    fsync() the temp file
    rename the temp file to the appropriate name
    fsync() the containing directory
    */

    // Perform blocking I/O on dedicated worker task
    tokio::task::spawn_blocking(move || {
        // Ensure directory exists
        create_dir_all(file_path.parent().unwrap())?;

        // Use a temporary extension
        file_path.set_extension("partial");

        // Open the file as direct as possible
        let mut options = OpenOptions::new();
        options.write(true).create(true);
        if cfg!(windows) || cfg!(unix) {
            direct_flag(&mut options);
        }
        let mut file = options.open(&file_path)?;

        // Write all data to file
        if let Err(e) = file.write_all(data.as_ref()) {
            _ = remove_file(&file_path);
            return Err(e);
        }

        // Sync everything
        if let Err(e) = file.sync_all() {
            _ = remove_file(&file_path);
            return Err(e);
        }

        // Rename the file
        let old_path = file_path.clone();
        file_path.set_extension("");
        if let Err(e) = std::fs::rename(&old_path, &file_path) {
            _ = remove_file(&old_path);
            return Err(e);
        }

        // No idea how to fsync the directory in portable Rust!!

        Ok(())
    })
}