use super::*;
use rand::random;
use std::{
    fs::{create_dir_all, remove_file, OpenOptions},
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

#[cfg(unix)]
use libc;
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

pub struct Cache {
    cache_root: PathBuf,
}

impl Cache {
    pub fn new(config: &settings::Config) -> Self {
        Self {
            cache_root: PathBuf::from(&config.cache_dir),
        }
    }

    pub async fn store(&self, data: &Arc<Vec<u8>>) -> Result<PathBuf, std::io::Error> {
        /*
        create a new temp file (on the same file system!)
        write data to the temp file
        fsync() the temp file
        rename the temp file to the appropriate name
        fsync() the containing directory
        */

        // Compose a subdirectory
        let sub_dir = [
            &(random::<u16>() % 4096).to_string(),
            &(random::<u16>() % 4096).to_string(),
            &(random::<u16>() % 4096).to_string(),
        ]
        .iter()
        .collect::<PathBuf>();

        // Concat full path
        let full_dir_path = [&self.cache_root, &sub_dir].iter().collect::<PathBuf>();

        // Perform blocking I/O on dedicated worker task
        let data = data.clone();
        tokio::task::spawn_blocking(move || {
            // Ensure directory exists
            create_dir_all(&full_dir_path)?;

            // Compose a filename
            let (mut file_name, file_path, mut file) = loop {
                let mut file_name = PathBuf::from(random::<u64>().to_string());

                // Write to temp file first
                file_name.set_extension("partial");

                // Open the file as direct as possible
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                if cfg!(windows) || cfg!(unix) {
                    direct_flag(&mut options);
                }

                let file_path = [&full_dir_path, &file_name].iter().collect::<PathBuf>();
                match options.open(&file_path) {
                    Ok(file) => break (file_name, file_path, file),
                    Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
                    _ => { /* Pick a new random name */ }
                }
            };

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
            let old_name = file_name.clone();
            file_name.set_extension("");
            if let Err(e) = std::fs::rename(&old_name, &file_name) {
                _ = remove_file(&file_path);
                return Err(e);
            }

            // No idea how to fsync the directory in portable Rust!!

            // Return the sub_dir path to the new file
            Ok([&sub_dir, &file_name].iter().collect::<PathBuf>())
        })
        .await?
    }
}
