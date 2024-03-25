use super::*;
use anyhow::anyhow;
use hardy_bpa_core::{storage::MetadataStorage, *};
use hardy_cbor as cbor;
use std::{collections::HashMap, fs::create_dir_all, path::PathBuf, sync::Arc};

pub struct Storage {
    connection: tokio::sync::Mutex<rusqlite::Connection>,
}

impl Storage {
    pub fn init(
        config: &HashMap<String, config::Value>,
        mut upgrade: bool,
    ) -> Result<std::sync::Arc<Self>, anyhow::Error> {
        let db_dir: String = config.get("db_dir").map_or_else(
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
                    .map_err(|e| anyhow!("'db_dir' is not a string value: {}!", e))
            },
        )?;

        // Compose DB name
        let file_path = [&db_dir, "metadata.db"].iter().collect::<PathBuf>();

        // Ensure directory exists
        create_dir_all(file_path.parent().unwrap())?;

        // Attempt to open existing database first
        let mut connection = match rusqlite::Connection::open_with_flags(
            &file_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(conn) => conn,
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::CannotOpen,
                    extended_code: _,
                },
                _,
            )) => {
                // Create database
                upgrade = true;
                rusqlite::Connection::open_with_flags(
                    &file_path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?
            }
            Err(e) => Err(e)?,
        };

        // Migrate the database to the latest schema
        migrate::migrate(&mut connection, upgrade)?;

        // Mark all existing bundles as unconfirmed
        connection.execute(
            r#"
            INSERT OR IGNORE INTO unconfirmed_bundles (bundle_id)
            SELECT id FROM bundles;
            "#,
            (),
        )?;

        Ok(Arc::new(Storage {
            connection: tokio::sync::Mutex::new(connection),
        }))
    }
}

impl MetadataStorage for Storage {
    fn check<F>(&self, f: F) -> Result<(), anyhow::Error>
    where
        F: FnMut(bundle::Bundle) -> Result<bool, anyhow::Error>,
    {
        todo!()
    }

    async fn store(
        &self,
        storage_name: &str,
        bundle: &bundle::Bundle,
    ) -> Result<(), anyhow::Error> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Insert bundle
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
                storage_name,
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

        // Insert extension blocks
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
        Ok(())
    }

    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error> {
        // Delete
        Ok(self
            .connection
            .lock()
            .await
            .prepare_cached(r#"DELETE FROM bundles WHERE file_name = ?1;"#)?
            .execute([storage_name])?
            != 0)
    }

    async fn confirm_exists(&self, storage_name: &str) -> Result<bool, anyhow::Error> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction()?;

        // Check if bundle exists
        let bundle_id: i64 = match trans
            .prepare_cached(r#"SELECT id FROM bundles WHERE file_name = ?1 LIMIT 1;"#)?
            .query_row([storage_name], |row| row.get(0))
        {
            Ok(bundle_id) => bundle_id,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
            Err(e) => Err(e)?,
        };

        // Remove from unconfirmed set
        trans
            .prepare_cached(r#"DELETE FROM unconfirmed_bundles WHERE bundle_id = ?1;"#)?
            .execute([bundle_id])?;
        Ok(true)
    }
}
