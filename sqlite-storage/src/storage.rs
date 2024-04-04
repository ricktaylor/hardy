use super::*;
use anyhow::anyhow;
use base64::prelude::*;
use hardy_bpa_core::{async_trait, bundle, storage::MetadataStorage};
use hardy_cbor as cbor;
use std::{collections::HashMap, fs::create_dir_all, path::PathBuf, sync::Arc};

pub struct Storage {
    connection: tokio::sync::Mutex<rusqlite::Connection>,
}

impl Storage {
    pub fn init(
        config: &HashMap<String, config::Value>,
        mut upgrade: bool,
    ) -> Result<std::sync::Arc<dyn MetadataStorage>, anyhow::Error> {
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

fn encode_eid(eid: &bundle::Eid) -> Result<rusqlite::types::Value, anyhow::Error> {
    match eid {
        bundle::Eid::Null => Ok(rusqlite::types::Value::Null),
        _ => Ok(rusqlite::types::Value::Blob(cbor::encode::write(eid))),
    }
}

fn decode_eid(
    row: &rusqlite::Row,
    idx: impl rusqlite::RowIndex,
) -> Result<bundle::Eid, anyhow::Error> {
    match row.get_ref(idx)? {
        rusqlite::types::ValueRef::Blob(b) => cbor::decode::parse(b),
        rusqlite::types::ValueRef::Null => Ok(bundle::Eid::Null),
        _ => Err(anyhow!("EID encoded as unusual sqlite type")),
    }
}

fn unpack_bundles(mut rows: rusqlite::Rows) -> Result<Vec<(i64, bundle::Bundle)>, anyhow::Error> {
    /* Expected query MUST look like:
           0:  bundles.id,
           1:  bundles.status,
           2:  bundles.file_name,
           3:  bundles.hash,
           4:  bundles.received_at,
           5:  bundles.flags,
           6:  bundles.source,
           7:  bundles.destination,
           8:  bundles.report_to,
           9:  bundles.creation_time,
           10: bundles.creation_seq_num,
           11: bundles.lifetime,
           12: bundle_fragments.offset,
           13: bundle_fragments.total_len,
           14: bundle_blocks.block_num,
           15: bundle_blocks.block_type,
           16: bundle_blocks.block_flags,
           17: bundle_blocks.data_offset
    */

    let mut bundles = Vec::new();
    let mut row_result = rows.next()?;
    while let Some(mut row) = row_result {
        let bundle_id: i64 = row.get(0)?;
        let metadata = bundle::Metadata {
            status: row.get::<usize, u64>(1)?.try_into()?,
            storage_name: row.get(2)?,
            hash: row.get(3)?,
            received_at: row.get(4)?,
        };
        let primary = bundle::PrimaryBlock {
            flags: row.get::<usize, u64>(5)?.into(),
            source: decode_eid(row, 6)?,
            destination: decode_eid(row, 7)?,
            report_to: decode_eid(row, 8)?,
            timestamp: (row.get(9)?, row.get(10)?),
            lifetime: row.get(11)?,
            fragment_info: match row.get_ref(12)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(offset) => Some(bundle::FragmentInfo {
                    offset: offset as u64,
                    total_len: row.get(13)?,
                }),
                _ => return Err(anyhow!("Fragment info is invalid")),
            },
        };

        let mut extensions = HashMap::new();
        loop {
            let block_number: u64 = row.get(14)?;
            let block = bundle::Block {
                block_type: row.get::<usize, u64>(15)?.try_into()?,
                flags: row.get::<usize, u64>(16)?.into(),
                data_offset: row.get(17)?,
            };

            if extensions.insert(block_number, block).is_some() {
                return Err(anyhow!("Duplicate block number in DB!"));
            }

            row_result = rows.next()?;
            row = match row_result {
                None => break,
                Some(row) => row,
            };

            if row.get::<usize, i64>(0)? != bundle_id {
                break;
            }
        }

        bundles.push((
            bundle_id,
            bundle::Bundle {
                metadata: Some(metadata),
                primary,
                extensions,
            },
        ));
    }
    Ok(bundles)
}

#[async_trait]
impl MetadataStorage for Storage {
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(bundle::Bundle) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error> {
        // Loop through subsets of 200 bundles, so we don't fill all memory
        loop {
            let bundles = unpack_bundles(
                self.connection
                    .blocking_lock()
                    .prepare(
                        r#"
                    WITH subset AS (
                        SELECT 
                            bundles.id AS id,
                            status,
                            file_name,
                            hash,
                            received_at,
                            flags,
                            source,
                            destination,
                            report_to,
                            creation_time,
                            creation_seq_num,
                            lifetime,                    
                            offset,
                            total_len
                        FROM unconfirmed_bundles
                        JOIN bundles ON bundles.id = unconfirmed_bundles.bundle_id
                        LEFT OUTER JOIN bundle_fragments ON bundle_fragments.bundle_id = bundles.id
                        LIMIT 200
                    )
                    SELECT 
                        subset.*,
                        block_num,
                        block_type,
                        block_flags,
                        data_offset
                    FROM subset
                    LEFT OUTER JOIN bundle_blocks ON bundle_blocks.id = subset.id;
                "#,
                    )?
                    .query([])?,
            )?;
            if bundles.is_empty() {
                break;
            }

            // Now enumerate the vector outside the query implicit transaction
            for (_bundle_id, bundle) in bundles {
                if !f(bundle)? {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn store(
        &self,
        storage_name: &str,
        hash: &[u8],
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
                hash,
                flags,
                destination,
                creation_time,
                creation_seq_num,
                lifetime,
                source,
                report_to)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9);"#,
            )?
            .insert((
                storage_name,
                BASE64_STANDARD.encode(hash),
                <bundle::BundleFlags as Into<u64>>::into(bundle.primary.flags),
                &encode_eid(&bundle.primary.destination)?,
                bundle.primary.timestamp.0,
                bundle.primary.timestamp.1,
                bundle.primary.lifetime,
                &encode_eid(&bundle.primary.source)?,
                &encode_eid(&bundle.primary.report_to)?,
            ))?;

        // Insert extension blocks
        let mut block_stmt = trans.prepare_cached(
            r#"
            INSERT INTO bundle_blocks (
                bundle_id,
                block_type,
                block_num,
                block_flags,
                data_offset)
            VALUES (?1,?2,?3,?4,?5);"#,
        )?;
        for (block_num, block) in &bundle.extensions {
            block_stmt.execute((
                bundle_id,
                <bundle::BlockType as Into<u64>>::into(block.block_type),
                block_num,
                <bundle::BlockFlags as Into<u64>>::into(block.flags),
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

    async fn confirm_exists(
        &self,
        storage_name: &str,
        hash: Option<&[u8]>,
    ) -> Result<bool, anyhow::Error> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction()?;

        // Check if bundle exists
        let bundle_id: i64 = match if let Some(hash) = hash {
            trans
                .prepare_cached(
                    r#"SELECT id FROM bundles WHERE file_name = ?1 AND hash = ?2 LIMIT 1;"#,
                )?
                .query_row([storage_name, &BASE64_STANDARD.encode(hash)], |row| {
                    row.get(0)
                })
        } else {
            trans
                .prepare_cached(r#"SELECT id FROM bundles WHERE file_name = ?1 LIMIT 1;"#)?
                .query_row([storage_name], |row| row.get(0))
        } {
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