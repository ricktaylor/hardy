use super::*;
use anyhow::anyhow;
use base64::prelude::*;
use hardy_bpa_core::{async_trait, bundle, storage::MetadataStorage};
use hardy_cbor as cbor;
use log_err::*;
use rusqlite::OptionalExtension;
use std::{
    collections::HashMap,
    fs::create_dir_all,
    path::Path,
    sync::{Arc, Mutex},
};
use tracing::instrument;

pub struct Storage {
    connection: Arc<Mutex<rusqlite::Connection>>,
}

fn bundle_status_to_pair(value: bundle::BundleStatus) -> (i64, Option<time::OffsetDateTime>) {
    match value {
        bundle::BundleStatus::IngressPending => (0, None),
        bundle::BundleStatus::DispatchPending => (1, None),
        bundle::BundleStatus::ReassemblyPending => (2, None),
        bundle::BundleStatus::CollectionPending => (3, None),
        bundle::BundleStatus::ForwardPending => (4, None),
        bundle::BundleStatus::Waiting(until) => (5, Some(until)),
        bundle::BundleStatus::Tombstone => (6, None),
    }
}

fn unpack_bundle_status(
    row: &rusqlite::Row,
    idx1: usize,
    idx2: usize,
) -> Result<bundle::BundleStatus, anyhow::Error> {
    match (
        row.get::<usize, i64>(idx1)?,
        row.get::<usize, Option<time::OffsetDateTime>>(idx2)?,
    ) {
        (0, None) => Ok(bundle::BundleStatus::IngressPending),
        (1, None) => Ok(bundle::BundleStatus::DispatchPending),
        (2, None) => Ok(bundle::BundleStatus::ReassemblyPending),
        (3, None) => Ok(bundle::BundleStatus::CollectionPending),
        (4, None) => Ok(bundle::BundleStatus::ForwardPending),
        (5, Some(until)) => Ok(bundle::BundleStatus::Waiting(until)),
        (6, None) => Ok(bundle::BundleStatus::Tombstone),
        (v, d) => Err(anyhow!("Invalid BundleStatus value {}/{:?}", v, d)),
    }
}

impl Storage {
    #[instrument(skip(config))]
    pub fn init(
        config: &HashMap<String, config::Value>,
        mut upgrade: bool,
    ) -> Result<Arc<dyn MetadataStorage>, anyhow::Error> {
        // Compose DB name
        let file_path = config
            .get("db_dir")
            .map_or_else(
                || {
                    directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME)
                        .map_or_else(
                            || {
                                if cfg!(unix) {
                                    Ok(Path::new("/var/spool").join(built_info::PKG_NAME))
                                } else {
                                    Err(anyhow!("Failed to resolve local store directory"))
                                }
                            },
                            |project_dirs| {
                                Ok(project_dirs.cache_dir().into())
                                // Lin: /home/alice/.store/barapp
                                // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\store
                                // Mac: /Users/Alice/Library/stores/com.Foo-Corp.Bar-App
                            },
                        )
                },
                |v| {
                    v.clone()
                        .into_string()
                        .map(|s| s.into())
                        .map_err(|e| anyhow!("'db_dir' is not a string value: {}!", e))
                },
            )?
            .join("metadata.db");

        log::info!("Using database: {}", file_path.display());

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

        // Mark all existing non-Tombstone bundles as unconfirmed
        connection.execute(
            r#"
            INSERT OR IGNORE INTO unconfirmed_bundles (bundle_id)
            SELECT id FROM bundles WHERE status != ?1;"#,
            [bundle_status_to_pair(bundle::BundleStatus::Tombstone).0],
        )?;

        // Create temporary tables for restarting
        connection.execute_batch(
            r#"
            CREATE TEMPORARY TABLE restart_bundles (
                bundle_id INTEGER UNIQUE NOT NULL
            ) STRICT;"#,
        )?;

        Ok(Arc::new(Storage {
            connection: Arc::new(Mutex::new(connection)),
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

// Quick helper for type conversion
#[inline]
fn as_u64(v: i64) -> u64 {
    v as u64
}

#[inline]
fn as_i64<T: Into<u64>>(v: T) -> i64 {
    let v: u64 = v.into();
    v as i64
}

fn unpack_bundles(
    mut rows: rusqlite::Rows,
) -> Result<Vec<(i64, bundle::Metadata, bundle::Bundle)>, anyhow::Error> {
    /* Expected query MUST look like:
           0:  bundles.id,
           1:  bundles.status,
           2:  bundles.storage_name,
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
           13: bundles.fragment_offset,
           14: bundles.fragment_total_len,
           15: bundles.previous_node,
           16: bundles.age,
           17: bundles.hop_count,
           18: bundles.hop_limit,
           19: bundles.wait_until,
           20: bundle_blocks.block_num,
           21: bundle_blocks.block_type,
           22: bundle_blocks.block_flags,
           23: bundle_blocks.block_crc_type,
           24: bundle_blocks.data_offset,
           25: bundle_blocks.data_len
    */

    let mut bundles = Vec::new();
    let mut row_result = rows.next()?;
    while let Some(mut row) = row_result {
        let bundle_id: i64 = row.get(0)?;
        let metadata = bundle::Metadata {
            status: unpack_bundle_status(row, 1, 19)?,
            storage_name: row.get(2)?,
            hash: BASE64_STANDARD_NO_PAD.decode(row.get::<usize, String>(3)?)?,
            received_at: row.get(4)?,
        };

        let fragment_info = {
            let offset: i64 = row.get(13)?;
            let total_len: i64 = row.get(14)?;
            if offset == -1 && total_len == -1 {
                None
            } else {
                Some(bundle::FragmentInfo {
                    offset: as_u64(offset),
                    total_len: as_u64(total_len),
                })
            }
        };

        let mut bundle = bundle::Bundle {
            id: bundle::BundleId {
                source: decode_eid(row, 7)?,
                timestamp: bundle::CreationTimestamp {
                    creation_time: as_u64(row.get(10)?),
                    sequence_number: as_u64(row.get(11)?),
                },
                fragment_info,
            },
            flags: as_u64(row.get(5)?).into(),
            crc_type: as_u64(row.get(6)?).try_into()?,
            destination: decode_eid(row, 8)?,
            report_to: decode_eid(row, 9)?,
            lifetime: as_u64(row.get(12)?),
            blocks: HashMap::new(),
            previous_node: match row.get_ref(15)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Blob(b) => Some(cbor::decode::parse(b)?),
                _ => return Err(anyhow!("EID encoded as unusual sqlite type")),
            },
            age: row.get::<usize, Option<i64>>(16)?.map(as_u64),
            hop_count: match row.get_ref(17)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(i) => Some(bundle::HopInfo {
                    count: as_u64(i),
                    limit: as_u64(row.get(18)?),
                }),
                _ => return Err(anyhow!("EID encoded as unusual sqlite type")),
            },
        };

        loop {
            let block_number = as_u64(row.get(20)?);
            let block = bundle::Block {
                block_type: as_u64(row.get(21)?).into(),
                flags: as_u64(row.get(23)?).into(),
                crc_type: as_u64(row.get(23)?).try_into()?,
                data_offset: as_u64(row.get(24)?) as usize,
                data_len: as_u64(row.get(25)?) as usize,
            };

            if bundle.blocks.insert(block_number, block).is_some() {
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

        bundles.push((bundle_id, metadata, bundle));
    }
    Ok(bundles)
}

fn complete_replace(
    trans: &rusqlite::Transaction<'_>,
    storage_name: &str,
    hash: &[u8],
) -> Result<Option<i64>, anyhow::Error> {
    // Update the hash
    let bundle_id = trans
        .prepare_cached(
            r#"WITH replacements AS (
            SELECT bundle_id,hash FROM replacement_bundles
            WHERE storage_name = ?1 AND hash = ?2
            LIMIT 1 
        )
        UPDATE bundles SET hash = (
            SELECT replacements.hash 
            FROM replacements 
            WHERE id = replacements.bundle_id
        )
        WHERE id IN (SELECT bundle_id FROM replacements)
        RETURNING id;"#,
        )?
        .query_row((storage_name, BASE64_STANDARD_NO_PAD.encode(hash)), |row| {
            row.get::<usize, i64>(0)
        })
        .optional()?;

    // Remove the replacement marker
    let Some(bundle_id) = bundle_id else {
        return Ok(None);
    };

    trans
        .prepare_cached(r#"DELETE FROM replacement_bundles WHERE bundle_id = ?1;"#)?
        .execute([bundle_id])
        .map(|_| Some(bundle_id))
        .map_err(|e| e.into())
}

#[async_trait]
impl MetadataStorage for Storage {
    #[instrument(skip_all)]
    fn check_orphans(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error> {
        // Loop through subsets of 16 bundles, so we don't fill all memory
        loop {
            let bundles = unpack_bundles(
                self.connection
                    .lock()
                    .log_expect("Failed to lock connection mutex")
                    .prepare_cached(
                        r#"WITH subset AS (
                            SELECT 
                                id,
                                status,
                                storage_name,
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
                                fragment_offset,
                                fragment_total_len,
                                previous_node,
                                age,
                                hop_count,
                                hop_limit,
                                wait_until
                            FROM unconfirmed_bundles
                            JOIN bundles ON id = unconfirmed_bundles.bundle_id
                            LIMIT 16
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
                        JOIN bundle_blocks ON bundle_blocks.id = subset.id;"#,
                    )?
                    .query(())?,
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

    #[instrument(skip_all)]
    fn restart(
        &self,
        f: &mut dyn FnMut(bundle::Metadata, bundle::Bundle) -> Result<bool, anyhow::Error>,
    ) -> Result<(), anyhow::Error> {
        // Create a temporary table (because DELETE RETURNING cannot be used as a CTE)
        self.connection
            .lock()
            .log_expect("Failed to lock connection mutex")
            .prepare(
                r#"CREATE TEMPORARY TABLE restart_subset (
                    bundle_id INTEGER UNIQUE NOT NULL
                ) STRICT;"#,
            )?
            .execute(())?;

        loop {
            // Loop through subsets of 16 bundles, so we don't fill all memory
            let mut conn = self
                .connection
                .lock()
                .log_expect("Failed to lock connection mutex");
            let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

            // Grab a subset, ordered by status descending
            trans
                .prepare_cached(
                    r#"INSERT INTO restart_subset (bundle_id)
                            SELECT id
                            FROM restart_bundles
                            JOIN bundles ON bundles.id = restart_bundles.bundle_id
                            ORDER BY bundles.status DESC
                            LIMIT 16;"#,
                )?
                .execute(())?;

            // Remove from restart the subset we are about to process
            if trans
                .prepare_cached(
                    r#"DELETE FROM restart_bundles WHERE bundle_id IN (
                            SELECT bundle_id FROM restart_subset
                        );"#,
                )?
                .execute(())?
                == 0
            {
                break;
            }

            // Now enum the bundles from the subset
            let bundles = unpack_bundles(
                trans
                    .prepare_cached(
                        r#"SELECT 
                            id,
                            status,
                            storage_name,
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
                            fragment_offset,
                            fragment_total_len,
                            previous_node,
                            age,
                            hop_count,
                            hop_limit,
                            wait_until,
                            block_num,
                            block_type,
                            block_flags,
                            block_crc_type,
                            data_offset,
                            data_len
                        FROM restart_subset
                        JOIN bundles ON bundles.id = restart_subset.bundle_id
                        JOIN bundle_blocks ON bundle_blocks.id = restart_subset.bundle_id;"#,
                    )?
                    .query(())?,
            )?;

            // Commit transaction and drop it
            trans.commit()?;
            drop(conn);

            // Now enumerate the vector outside the transaction
            for (_bundle_id, metadata, bundle) in bundles {
                if !f(metadata, bundle)? {
                    break;
                }
            }
        }

        // And finally drop the restart tables - they're no longer required
        self.connection
            .lock()
            .log_expect("Failed to lock connection mutex")
            .execute_batch(
                r#"
                DROP TABLE temp.restart_subset;
                DROP TABLE temp.restart_bundles;"#,
            )
            .map_err(|e| e.into())
    }

    #[instrument(skip(self))]
    async fn store(
        &self,
        metadata: &bundle::Metadata,
        bundle: &bundle::Bundle,
    ) -> Result<bool, anyhow::Error> {
        let mut conn = self
            .connection
            .lock()
            .log_expect("Failed to lock connection mutex");
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (status, until) = bundle_status_to_pair(metadata.status);

        // Insert bundle
        let bundle_id = trans
            .prepare_cached(
                r#"
            INSERT OR IGNORE INTO bundles (
                status,
                storage_name,
                hash,
                flags,
                crc_type,
                source,
                destination,
                report_to,
                creation_time,
                creation_seq_num,
                lifetime,
                fragment_offset,
                fragment_total_len,
                previous_node,
                age,
                hop_count,
                hop_limit,
                wait_until
                )
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
            RETURNING id;"#,
            )?
            .query_row(
                rusqlite::params!(
                    status,
                    &metadata.storage_name,
                    BASE64_STANDARD_NO_PAD.encode(&metadata.hash),
                    as_i64(bundle.flags),
                    as_i64(bundle.crc_type),
                    &encode_eid(&bundle.id.source)?,
                    &encode_eid(&bundle.destination)?,
                    &encode_eid(&bundle.report_to)?,
                    as_i64(bundle.id.timestamp.creation_time),
                    as_i64(bundle.id.timestamp.sequence_number),
                    as_i64(bundle.lifetime),
                    bundle.id.fragment_info.map_or(-1, |f| as_i64(f.offset)),
                    bundle.id.fragment_info.map_or(-1, |f| as_i64(f.total_len)),
                    bundle
                        .previous_node
                        .as_ref()
                        .map_or(Ok(None), |p| encode_eid(p).map(Some))?,
                    bundle.age.map(as_i64),
                    bundle.hop_count.map(|h| as_i64(h.count)),
                    bundle.hop_count.map(|h| as_i64(h.limit)),
                    until
                ),
                |row| Ok(as_u64(row.get(0)?)),
            )
            .optional()?;

        // Insert extension blocks
        if let Some(bundle_id) = bundle_id {
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
                    as_i64(block.block_type),
                    as_i64(*block_num),
                    as_i64(block.flags),
                    as_i64(block.crc_type),
                    as_i64(block.data_offset as u64),
                    as_i64(block.data_len as u64),
                ))?;
            }
        }

        // Commit transaction
        trans
            .commit()
            .map(|_| bundle_id.is_some())
            .map_err(|e| e.into())
    }

    #[instrument(skip(self))]
    async fn remove(&self, storage_name: &str) -> Result<bool, anyhow::Error> {
        // Delete
        self.connection
            .lock()
            .log_expect("Failed to lock connection mutex")
            .prepare_cached(r#"DELETE FROM bundles WHERE storage_name = ?1;"#)?
            .execute([storage_name])
            .map(|count| count != 0)
            .map_err(|e| e.into())
    }

    #[instrument(skip(self))]
    async fn confirm_exists(&self, storage_name: &str, hash: &[u8]) -> Result<bool, anyhow::Error> {
        let mut conn = self
            .connection
            .lock()
            .log_expect("Failed to lock connection mutex");
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Check if bundle exists
        let Some(bundle_id) = trans
            .prepare_cached(
                r#"SELECT id FROM bundles WHERE storage_name = ?1 AND hash = ?2 LIMIT 1;"#,
            )?
            .query_row(
                (storage_name, &BASE64_STANDARD_NO_PAD.encode(hash)),
                |row| row.get::<usize, i64>(0),
            )
            .optional()?
            .map_or_else(
                || complete_replace(&trans, storage_name, hash),
                |bundle_id| Ok(Some(bundle_id)),
            )?
        else {
            return Ok(false);
        };

        // Remove from unconfirmed set
        if trans
            .prepare_cached(r#"DELETE FROM unconfirmed_bundles WHERE bundle_id = ?1;"#)?
            .execute([bundle_id])?
            != 0
        {
            // Add to restart set
            trans
                .prepare_cached(r#"INSERT INTO restart_bundles (bundle_id) VALUES (?1);"#)?
                .execute([bundle_id])?;

            trans.commit()?;
        }
        Ok(true)
    }

    #[instrument(skip(self))]
    async fn set_bundle_status(
        &self,
        storage_name: &str,
        status: bundle::BundleStatus,
    ) -> Result<bool, anyhow::Error> {
        let (status, until) = bundle_status_to_pair(status);
        self.connection
            .lock()
            .log_expect("Failed to lock connection mutex")
            .prepare_cached(
                r#"UPDATE bundles SET status = ?1, wait_until = ?2 WHERE storage_name = ?3;"#,
            )?
            .execute((status, until, storage_name))
            .map(|count| count != 0)
            .map_err(|e| e.into())
    }

    #[instrument(skip(self))]
    async fn begin_replace(&self, storage_name: &str, hash: &[u8]) -> Result<bool, anyhow::Error> {
        self.connection
            .lock()
            .log_expect("Failed to lock connection mutex")
            .prepare_cached(
                r#"INSERT INTO replacement_bundles (bundle_id, hash)
                SELECT id,?1 FROM bundles WHERE storage_name = ?2;"#,
            )?
            .execute((BASE64_STANDARD_NO_PAD.encode(hash), storage_name))
            .map(|count| count != 0)
            .map_err(|e| e.into())
    }

    #[instrument(skip(self))]
    async fn commit_replace(&self, storage_name: &str, hash: &[u8]) -> Result<bool, anyhow::Error> {
        let mut conn = self
            .connection
            .lock()
            .log_expect("Failed to lock connection mutex");
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        if complete_replace(&trans, storage_name, hash)?.is_none() {
            return Ok(false);
        };

        // Commit the transaction
        trans.commit().map(|_| true).map_err(|e| e.into())
    }

    #[instrument(skip(self))]
    async fn get_waiting_bundles(
        &self,
        limit: time::OffsetDateTime,
    ) -> Result<Vec<(bundle::Metadata, bundle::Bundle, time::OffsetDateTime)>, anyhow::Error> {
        unpack_bundles(
            self.connection
                .lock()
                .log_expect("Failed to lock connection mutex")
                .prepare_cached(
                    r#"SELECT 
                        id,
                        status,
                        storage_name,
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
                        fragment_offset,
                        fragment_total_len,
                        previous_node,
                        age,
                        hop_count,
                        hop_limit,
                        wait_until,
                        block_num,
                        block_type,
                        block_flags,
                        block_crc_type,
                        data_offset,
                        data_len
                    FROM bundles
                    JOIN bundle_blocks ON bundle_blocks.id = bundles.bundle_id
                    WHERE wait_until IS NOT NULL && unixepoch(wait_until) < unixepoch(?1)
                    LIMIT 256;"#,
                )?
                .query([limit])?,
        )
        .map(|v| {
            v.into_iter()
                .filter_map(|(_, metadata, bundle)| {
                    if let bundle::BundleStatus::Waiting(until) = &metadata.status {
                        let until = *until;
                        Some((metadata, bundle, until))
                    } else {
                        None
                    }
                })
                .collect()
        })
    }
}
