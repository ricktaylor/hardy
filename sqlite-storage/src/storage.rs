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
        _ => Ok(rusqlite::types::Value::Blob(cbor::encode::emit(eid))),
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

fn unpack_bundles(
    mut rows: rusqlite::Rows,
) -> Result<Vec<(i64, bundle::Metadata, bundle::Bundle)>, anyhow::Error> {
    /* Expected query MUST look like:
           0:  bundles.id,
           1:  bundles.status,
           2:  bundles.file_name,
           3:  bundles.hash,
           4:  bundles.received_at,
           5:  bundles.flags,
           6:  bundles.crc_type,
           7:  bundles.source,
           8:  bundles.destination,
           9:  bundles.report_to,
           10: bundles.creation_time,
           11: bundles.creation_seq_num,
           12: bundles.lifetime,
           13: bundle_fragments.offset,
           14: bundle_fragments.total_len,
           15: bundle_blocks.block_num,
           16: bundle_blocks.block_type,
           17: bundle_blocks.block_flags,
           18: bundle_blocks.block_crc_type,
           19: bundle_blocks.data_offset,
           20: bundle_blocks.data_len
    */

    let mut bundles = Vec::new();
    let mut row_result = rows.next()?;
    while let Some(mut row) = row_result {
        let bundle_id: i64 = row.get(0)?;
        let metadata = bundle::Metadata {
            status: row.get::<usize, u64>(1)?.try_into()?,
            storage_name: row.get(2)?,
            hash: BASE64_STANDARD.decode(row.get::<usize, String>(3)?)?,
            received_at: row.get(4)?,
        };
        let primary = bundle::PrimaryBlock {
            flags: row.get::<usize, u64>(5)?.into(),
            crc_type: row.get::<usize, u64>(6)?.try_into()?,
            source: decode_eid(row, 7)?,
            destination: decode_eid(row, 8)?,
            report_to: decode_eid(row, 9)?,
            timestamp: (row.get(10)?, row.get(11)?),
            lifetime: row.get(12)?,
            fragment_info: match row.get_ref(13)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(offset) => Some(bundle::FragmentInfo {
                    offset: offset as u64,
                    total_len: row.get(14)?,
                }),
                _ => return Err(anyhow!("Fragment info is invalid")),
            },
        };

        let mut blocks = HashMap::new();
        loop {
            let block_number: u64 = row.get(15)?;
            let block = bundle::Block {
                block_type: row.get::<usize, u64>(16)?.try_into()?,
                flags: row.get::<usize, u64>(17)?.into(),
                crc_type: row.get::<usize, u64>(18)?.try_into()?,
                data_offset: row.get(19)?,
                data_len: row.get(20)?,
            };

            if blocks.insert(block_number, block).is_some() {
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

        bundles.push((bundle_id, metadata, bundle::Bundle { primary, blocks }));
    }
    Ok(bundles)
}

#[async_trait]
impl MetadataStorage for Storage {
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool, anyhow::Error>,
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
                            crc_type,
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
                        block_crc_type,
                        data_offset,
                        data_len
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
            for (_bundle_id, metadata, bundle) in bundles {
                if !f(metadata, bundle)? {
                    break;
                }
            }
        }
        Ok(())
    }

    async fn store(
        &self,
        status: bundle::BundleStatus,
        storage_name: &str,
        hash: &[u8],
        bundle: &bundle::Bundle,
    ) -> Result<bundle::Metadata, anyhow::Error> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Insert bundle
        let (bundle_id, received_at) = trans
            .prepare_cached(
                r#"
            INSERT INTO bundles (
                status,
                file_name,
                hash,
                flags,
                crc_type,
                destination,
                creation_time,
                creation_seq_num,
                lifetime,
                source,
                report_to)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
            RETURNING id,received_at;"#,
            )?
            .query_row(
                (
                    <bundle::BundleStatus as Into<u64>>::into(status),
                    storage_name,
                    BASE64_STANDARD.encode(hash),
                    <bundle::BundleFlags as Into<u64>>::into(bundle.primary.flags),
                    <bundle::CrcType as Into<u64>>::into(bundle.primary.crc_type),
                    &encode_eid(&bundle.primary.destination)?,
                    bundle.primary.timestamp.0,
                    bundle.primary.timestamp.1,
                    bundle.primary.lifetime,
                    &encode_eid(&bundle.primary.source)?,
                    &encode_eid(&bundle.primary.report_to)?,
                ),
                |row| {
                    Ok((
                        row.get::<usize, u64>(0)?,
                        row.get::<usize, Option<time::OffsetDateTime>>(1)?,
                    ))
                },
            )?;

        // Insert extension blocks
        let mut block_stmt = trans.prepare_cached(
            r#"
            INSERT INTO bundle_blocks (
                bundle_id,
                block_type,
                block_num,
                block_flags,
                block_crc_type,
                data_offset,
                data_len)
            VALUES (?1,?2,?3,?4,?5,?6);"#,
        )?;
        for (block_num, block) in &bundle.blocks {
            block_stmt.execute((
                bundle_id,
                <bundle::BlockType as Into<u64>>::into(block.block_type),
                block_num,
                <bundle::BlockFlags as Into<u64>>::into(block.flags),
                <bundle::CrcType as Into<u64>>::into(block.crc_type),
                block.data_offset,
                block.data_len,
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
        Ok(bundle::Metadata {
            status,
            storage_name: storage_name.to_string(),
            hash: hash.to_vec(),
            received_at,
        })
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
