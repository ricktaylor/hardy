use super::*;
use rand::random;
use std::{
    collections::HashSet,
    fs::{create_dir_all, remove_file, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
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
    db: database::Database,
}

impl Cache {
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
        let bundle_result = match bundle::parse(data) {
            Ok(bundle) => self.insert_bundle(file_path.as_path(), bundle).await,
            Err(e) => Err(e),
        };

        // Await the result of write_bundle
        let write_result = match write_handle.await {
            Err(e) => Err(e.into()),
            Ok(Err(e)) => Err(e),
            Ok(Ok(())) => Ok(()),
        };

        // Always remove partials entry
        self.partials.lock().unwrap().remove(&file_path);

        // Check result of write_bundle
        if let Err(e) = write_result {
            if let Ok((bundle_id, _)) = bundle_result {
                // Delete bundle from db
                todo!();
            }
            return Err(e.into());
        }

        // Check result of bundle parse
        match bundle_result {
            Ok((bundle_id, bundle)) => {
                // Insert bundle into pipeline
                todo!();

                // No failure
                Ok(None)
            }
            Err(e) => {
                // Remove the cached file
                _ = tokio::fs::remove_file(&file_path).await;

                // Reply with forwarding failure - NOT an error
                Ok(Some(format!("Bundle validation failed: {}", e)))
            }
        }
    }

    async fn insert_bundle(
        &self,
        file_path: &Path,
        bundle: bundle::Bundle,
    ) -> Result<(i64, bundle::Bundle), anyhow::Error> {
        let mut conn = self.db.lock().await;
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let bundle_id = trans
            .prepare_cached(
                r#"
            INSERT INTO bundles (
                file_name,
                flags,
                destination,
                creation_time,
                creation_seq_num,
                lifetime,
                source,
                report_to)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8);"#,
            )?
            .insert((
                file_path.to_string_lossy(),
                bundle.primary.flags.as_u64(),
                cbor::encode::write(&bundle.primary.destination),
                bundle.primary.timestamp.0,
                bundle.primary.timestamp.1,
                bundle.primary.lifetime,
                if let bundle::Eid::Null = bundle.primary.source {
                    None
                } else {
                    Some(cbor::encode::write(&bundle.primary.source))
                },
                if let bundle::Eid::Null = bundle.primary.report_to {
                    None
                } else {
                    Some(cbor::encode::write(&bundle.primary.report_to))
                },
            ))?;

        let mut block_query = trans.prepare_cached(
            r#"
            INSERT INTO bundle_blocks (
                bundle_id,
                block_type,
                block_num,
                flags,
                data_offset)
            VALUES (?1,?2,?3,?4,?5);"#,
        )?;

        // Insert extension blocks
        for (block_num, block) in &bundle.extensions {
            block_query.execute((
                bundle_id,
                block.block_type.as_u64(),
                block_num,
                block.flags.as_u64(),
                block.data_offset,
            ))?;
        }

        // Insert fragments
        if let Some(fragment_info) = &bundle.primary.fragment_info {
            trans
                .prepare_cached(
                    r#"
                INSERT INTO bundle_fragments (
                    bundle_id,
                    offset,
                    total_len)
                VALUES (?1,?2,?3);"#,
                )?
                .execute((bundle_id, fragment_info.offset, fragment_info.total_len))?;
        }
        Ok((bundle_id, bundle))
    }

    async fn check(&mut self, cancel_token: CancellationToken) -> bool {
        // Walk directories checking if the bundle is in the db
        log::info!("Checking cache...");

        todo!();

        log::info!("Cache check complete");
        true
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

        // No idea how to fsync the directory in portable Rust!

        Ok(())
    })
}

pub async fn init(
    config: &settings::Config,
    db: database::Database,
    cancel_token: CancellationToken,
) -> Option<Cache> {
    let mut cache = Cache {
        cache_root: PathBuf::from(&config.cache_dir),
        partials: Arc::new(Mutex::new(HashSet::new())),
        db,
    };

    if !cache.check(cancel_token).await {
        None
    } else {
        Some(cache)
    }
}
