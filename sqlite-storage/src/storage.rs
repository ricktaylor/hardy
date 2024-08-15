use super::*;
use base64::prelude::*;
use hardy_bpa_api::{async_trait, metadata, storage};
use hardy_bpv7::prelude as bpv7;
use hardy_cbor as cbor;
use rusqlite::OptionalExtension;
use std::{collections::HashMap, path::Path, sync::Arc};
use thiserror::Error;
use tokio::sync::Mutex;
use trace_err::*;
use tracing::*;

pub struct Storage {
    connection: Arc<Mutex<rusqlite::Connection>>,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("No such bundle")]
    NotFound,
}

#[derive(Debug)]
#[repr(i64)]
enum StatusCodes {
    IngressPending = 0,
    DispatchPending = 1,
    ReassemblyPending = 2,
    CollectionPending = 3,
    ForwardPending = 4,
    ForwardAckPending = 5,
    Waiting = 6,
    Tombstone = 7,
}

impl From<i64> for StatusCodes {
    fn from(value: i64) -> Self {
        match value {
            0 => Self::IngressPending,
            1 => Self::DispatchPending,
            2 => Self::ReassemblyPending,
            3 => Self::CollectionPending,
            4 => Self::ForwardPending,
            5 => Self::ForwardAckPending,
            6 => Self::Waiting,
            7 => Self::Tombstone,
            _ => panic!("Invalid BundleStatus value {value}"),
        }
    }
}

impl From<StatusCodes> for i64 {
    fn from(value: StatusCodes) -> Self {
        value as i64
    }
}

fn bundle_status_to_parts(
    value: &metadata::BundleStatus,
) -> (i64, Option<i64>, Option<time::OffsetDateTime>) {
    match value {
        metadata::BundleStatus::IngressPending => (StatusCodes::IngressPending.into(), None, None),
        metadata::BundleStatus::DispatchPending => {
            (StatusCodes::DispatchPending.into(), None, None)
        }
        metadata::BundleStatus::ReassemblyPending => {
            (StatusCodes::ReassemblyPending.into(), None, None)
        }
        metadata::BundleStatus::CollectionPending => {
            (StatusCodes::CollectionPending.into(), None, None)
        }
        metadata::BundleStatus::ForwardPending => (StatusCodes::ForwardPending.into(), None, None),
        metadata::BundleStatus::ForwardAckPending(handle, until) => (
            StatusCodes::ForwardAckPending.into(),
            Some(*handle as i64),
            Some(*until),
        ),
        metadata::BundleStatus::Waiting(until) => (StatusCodes::Waiting.into(), None, Some(*until)),
        metadata::BundleStatus::Tombstone(from) => {
            (StatusCodes::Tombstone.into(), None, Some(*from))
        }
    }
}

fn columns_to_bundle_status(
    row: &rusqlite::Row,
    idx1: usize,
    idx2: usize,
    idx3: usize,
) -> rusqlite::Result<metadata::BundleStatus> {
    match (
        row.get::<_, i64>(idx1)?.into(),
        row.get::<_, Option<i64>>(idx2)?,
        row.get::<_, Option<time::OffsetDateTime>>(idx3)?,
    ) {
        (StatusCodes::IngressPending, None, None) => Ok(metadata::BundleStatus::IngressPending),
        (StatusCodes::DispatchPending, None, None) => Ok(metadata::BundleStatus::DispatchPending),
        (StatusCodes::ReassemblyPending, None, None) => {
            Ok(metadata::BundleStatus::ReassemblyPending)
        }
        (StatusCodes::CollectionPending, None, None) => {
            Ok(metadata::BundleStatus::CollectionPending)
        }
        (StatusCodes::ForwardPending, None, None) => Ok(metadata::BundleStatus::ForwardPending),
        (StatusCodes::ForwardAckPending, Some(handle), Some(until)) => Ok(
            metadata::BundleStatus::ForwardAckPending(handle as u32, until),
        ),
        (StatusCodes::Waiting, None, Some(until)) => Ok(metadata::BundleStatus::Waiting(until)),
        (StatusCodes::Tombstone, None, Some(from)) => Ok(metadata::BundleStatus::Tombstone(from)),
        (v, t, d) => panic!("Invalid BundleStatus value combination {v:?}/{t:?}/{d:?}"),
    }
}

impl Storage {
    #[instrument(skip(config))]
    pub fn init(
        config: &HashMap<String, config::Value>,
        mut upgrade: bool,
    ) -> Arc<dyn storage::MetadataStorage> {
        // Compose DB name
        let file_path = config
            .get("db_dir")
            .map_or_else(
                || {
                    directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME)
                        .map_or_else(
                            || {
                                cfg_if::cfg_if! {
                                    if #[cfg(unix)] {
                                        Path::new("/var/spool").join(built_info::PKG_NAME)
                                    } else if #[cfg(windows)] {
                                        std::env::current_exe().join(built_info::PKG_NAME)
                                    } else {
                                        compile_error!("No idea how to determine default local store directory for target platform")
                                    }
                                }
                            },
                            |project_dirs| {
                                project_dirs.cache_dir().into()
                                // Lin: /home/alice/.store/barapp
                                // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\store
                                // Mac: /Users/Alice/Library/stores/com.Foo-Corp.Bar-App
                            },
                        )
                },
                |v| {
                    v.clone()
                        .into_string().trace_expect("Invalid 'db_dir' value in configuration").into()
                },
            )
            .join("metadata.db");

        info!("Using database: {}", file_path.display());

        // Ensure directory exists
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).trace_expect(&format!(
                "Failed to create metadata store directory {}",
                parent.display()
            ));
        }

        // Attempt to open existing database first
        let mut connection = match rusqlite::Connection::open_with_flags(
            &file_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::CannotOpen,
                    ..
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
                )
            }
            r => r,
        }
        .trace_expect("Failed to open metadata store database");

        // Migrate the database to the latest schema
        migrate::migrate(&mut connection, upgrade)
            .trace_expect("Failed to migrate metadata store database");

        // Mark all existing non-Tombstone bundles as unconfirmed
        connection
            .execute(
                r#"
            INSERT OR IGNORE INTO unconfirmed_bundles (bundle_id)
            SELECT id FROM bundles WHERE status != ?1;"#,
                [StatusCodes::Tombstone as i64],
            )
            .trace_expect("Failed to prepare metadata store database");

        Arc::new(Storage {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    async fn sync_conn<F, R>(&self, f: F) -> storage::Result<R>
    where
        F: Fn(&mut rusqlite::Connection) -> storage::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.connection.clone();
        tokio::task::spawn_blocking(move || f(&mut conn.blocking_lock()))
            .await
            .map_err(Into::<storage::Error>::into)?
    }
}

fn encode_eid(eid: &bpv7::Eid) -> rusqlite::types::Value {
    rusqlite::types::Value::Blob(cbor::encode::emit(eid))
}

fn decode_eid(
    row: &rusqlite::Row,
    idx: impl rusqlite::RowIndex,
) -> Result<bpv7::Eid, Box<dyn std::error::Error + Send + Sync>> {
    let rusqlite::types::ValueRef::Blob(b) = row.get_ref(idx)? else {
        panic!("EID encoded as unusual sqlite type")
    };
    cbor::decode::parse(b).map_err(Into::into)
}

fn encode_creation_time(timestamp: Option<bpv7::DtnTime>) -> i64 {
    if let Some(timestamp) = timestamp {
        as_i64(timestamp.millisecs())
    } else {
        0
    }
}

fn decode_creation_time(
    row: &rusqlite::Row,
    idx: usize,
) -> rusqlite::Result<Option<bpv7::DtnTime>> {
    let timestamp = row.get::<_, i64>(idx)?;
    if timestamp == 0 {
        Ok(None)
    } else {
        Ok(Some(bpv7::DtnTime::new(as_u64(timestamp))))
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

fn unpack_bundles(mut rows: rusqlite::Rows<'_>, tx: &storage::Sender) -> storage::Result<()> {
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
           20: bundles.ack_handle,
           21: bundle_blocks.block_num,
           22: bundle_blocks.block_type,
           23: bundle_blocks.block_flags,
           24: bundle_blocks.block_crc_type,
           25: bundle_blocks.data_offset,
           26: bundle_blocks.data_len
    */

    while let Some(mut row) = rows.next()? {
        let bundle_id: i64 = row.get(0)?;
        let metadata = metadata::Metadata {
            status: columns_to_bundle_status(row, 1, 20, 19)?,
            storage_name: row.get(2)?,
            hash: BASE64_STANDARD_NO_PAD
                .decode(row.get::<_, String>(3)?)
                .trace_expect("Failed to base64 decode hash")
                .into(),
            received_at: row.get(4)?,
        };

        let fragment_info = {
            let offset: i64 = row.get(13)?;
            let total_len: i64 = row.get(14)?;
            if offset == -1 && total_len == -1 {
                None
            } else {
                Some(bpv7::FragmentInfo {
                    offset: as_u64(offset),
                    total_len: as_u64(total_len),
                })
            }
        };

        let mut bundle = bpv7::Bundle {
            id: bpv7::BundleId {
                source: decode_eid(row, 7)?,
                timestamp: bpv7::CreationTimestamp {
                    creation_time: decode_creation_time(row, 10)?,
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
                v => panic!("EID encoded as unusual sqlite type: {:?}", v),
            },
            age: row.get::<_, Option<i64>>(16)?.map(as_u64),
            hop_count: match row.get_ref(17)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Integer(i) => Some(bpv7::HopInfo {
                    count: as_u64(i),
                    limit: as_u64(row.get(18)?),
                }),
                v => panic!("EID encoded as unusual sqlite type: {:?}", v),
            },
        };

        loop {
            let block_number = as_u64(row.get(21)?);
            let block = bpv7::Block {
                block_type: as_u64(row.get(22)?).into(),
                flags: as_u64(row.get(23)?).into(),
                crc_type: as_u64(row.get(24)?).try_into()?,
                data_offset: as_u64(row.get(25)?) as usize,
                data_len: as_u64(row.get(26)?) as usize,
            };

            if bundle.blocks.insert(block_number, block).is_some() {
                panic!("Duplicate block number {block_number} in DB!");
            }

            row = match rows.next()? {
                None => break,
                Some(row) => row,
            };

            if row.get::<_, i64>(0)? != bundle_id {
                break;
            }
        }

        if tx
            .blocking_send(metadata::Bundle { bundle, metadata })
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

fn complete_replace(
    trans: &rusqlite::Transaction<'_>,
    storage_name: &str,
    hash: &[u8],
) -> storage::Result<Option<i64>> {
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
            row.get::<_, i64>(0)
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
        .map_err(Into::into)
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    #[instrument(skip(self))]
    async fn load(&self, bundle_id: &bpv7::BundleId) -> storage::Result<Option<metadata::Bundle>> {
        let bundle_id = bundle_id.clone();
        self.sync_conn(move |conn| {
            let mut stmt = conn.prepare_cached(
                r#"SELECT 
                    bundles.id,
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
                    ack_handle,
                    block_num,
                    block_type,
                    block_flags,
                    block_crc_type,
                    data_offset,
                    data_len
                FROM bundles
                JOIN bundle_blocks ON bundle_blocks.bundle_id = bundles.id
                WHERE 
                    source = ?1 AND
                    creation_time = ?2 AND
                    creation_seq_num = ?3 AND
                    fragment_offset = ?4 AND 
                    fragment_total_len = ?5
                LIMIT 1;"#,
            )?;

            let mut rows = stmt.query((
                encode_eid(&bundle_id.source),
                encode_creation_time(bundle_id.timestamp.creation_time),
                as_i64(bundle_id.timestamp.sequence_number),
                bundle_id.fragment_info.map_or(-1, |f| as_i64(f.offset)),
                bundle_id.fragment_info.map_or(-1, |f| as_i64(f.total_len)),
            ))?;

            let Some(mut row) = rows.next()? else {
                return Ok(None);
            };

            let bundle_id: i64 = row.get(0)?;
            let metadata = metadata::Metadata {
                status: columns_to_bundle_status(row, 1, 20, 19)?,
                storage_name: row.get(2)?,
                hash: BASE64_STANDARD_NO_PAD
                    .decode(row.get::<_, String>(3)?)?
                    .into(),
                received_at: row.get(4)?,
            };

            let fragment_info = {
                let offset: i64 = row.get(13)?;
                let total_len: i64 = row.get(14)?;
                if offset == -1 && total_len == -1 {
                    None
                } else {
                    Some(bpv7::FragmentInfo {
                        offset: as_u64(offset),
                        total_len: as_u64(total_len),
                    })
                }
            };

            let mut bundle = bpv7::Bundle {
                id: bpv7::BundleId {
                    source: decode_eid(row, 7)?,
                    timestamp: bpv7::CreationTimestamp {
                        creation_time: decode_creation_time(row, 10)?,
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
                    v => panic!("EID encoded as unusual sqlite type: {:?}", v),
                },
                age: row.get::<_, Option<i64>>(16)?.map(as_u64),
                hop_count: match row.get_ref(17)? {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Integer(i) => Some(bpv7::HopInfo {
                        count: as_u64(i),
                        limit: as_u64(row.get(18)?),
                    }),
                    v => panic!("EID encoded as unusual sqlite type: {:?}", v),
                },
            };

            loop {
                let block_number = as_u64(row.get(21)?);
                let block = bpv7::Block {
                    block_type: as_u64(row.get(22)?).into(),
                    flags: as_u64(row.get(23)?).into(),
                    crc_type: as_u64(row.get(24)?).try_into()?,
                    data_offset: as_u64(row.get(25)?) as usize,
                    data_len: as_u64(row.get(26)?) as usize,
                };

                if bundle.blocks.insert(block_number, block).is_some() {
                    panic!("Duplicate block number {block_number} in DB!");
                }

                row = match rows.next()? {
                    None => break,
                    Some(row) => row,
                };

                if row.get::<_, i64>(0)? != bundle_id {
                    panic!("More than one bundle in query!");
                }
            }
            Ok(Some(metadata::Bundle { bundle, metadata }))
        })
        .await
    }

    #[instrument(skip(self))]
    async fn store(
        &self,
        metadata: &metadata::Metadata,
        bundle: &bpv7::Bundle,
    ) -> storage::Result<bool> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (status, ack_handle, until) = bundle_status_to_parts(&metadata.status);

        // Insert bundle
        let bundle_id = trans
            .prepare_cached(
                r#"
            INSERT INTO bundles (
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
                wait_until,
                ack_handle
                )
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
            RETURNING id;"#,
            )?
            .query_row(
                rusqlite::params!(
                    status,
                    &metadata.storage_name,
                    BASE64_STANDARD_NO_PAD.encode(&metadata.hash),
                    as_i64(bundle.flags),
                    as_i64(bundle.crc_type),
                    encode_eid(&bundle.id.source),
                    encode_eid(&bundle.destination),
                    encode_eid(&bundle.report_to),
                    encode_creation_time(bundle.id.timestamp.creation_time),
                    as_i64(bundle.id.timestamp.sequence_number),
                    as_i64(bundle.lifetime),
                    bundle.id.fragment_info.map_or(-1, |f| as_i64(f.offset)),
                    bundle.id.fragment_info.map_or(-1, |f| as_i64(f.total_len)),
                    bundle.previous_node.as_ref().map(encode_eid),
                    bundle.age.map(as_i64),
                    bundle.hop_count.map(|h| as_i64(h.count)),
                    bundle.hop_count.map(|h| as_i64(h.limit)),
                    until,
                    ack_handle
                ),
                |row| Ok(as_u64(row.get(0)?)),
            );

        let bundle_id = match bundle_id {
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.extended_code == 2067 => {
                return Ok(false)
            }
            bundle_id => bundle_id.trace_expect("Failed to load bundle metadata"),
        };

        {
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
                    VALUES (?1,?2,?3,?4,?5,?6,?7);"#,
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
        trans.commit().map(|_| true).map_err(Into::into)
    }

    #[instrument(skip(self))]
    async fn remove(&self, storage_name: &str) -> storage::Result<()> {
        // Delete
        if !self
            .connection
            .lock()
            .await
            .prepare_cached(r#"DELETE FROM bundles WHERE storage_name = ?1;"#)?
            .execute([storage_name])
            .map(|count| count != 0)?
        {
            Err(Error::NotFound.into())
        } else {
            Ok(())
        }
    }

    #[instrument(skip(self))]
    async fn confirm_exists(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<metadata::Metadata>> {
        let bundle_id = bundle_id.clone();
        self.sync_conn(move |conn| {
            let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

            // Check if bundle exists
            let Some((bundle_id, metadata)) = trans
                .prepare_cached(
                    r#"SELECT 
                            id,
                            status,
                            ack_handle,
                            wait_until,
                            storage_name,
                            hash,
                            received_at
                        FROM bundles
                        WHERE 
                            source = ?1 AND
                            creation_time = ?2 AND
                            creation_seq_num = ?3 AND
                            fragment_offset = ?4 AND 
                            fragment_total_len = ?5
                        LIMIT 1;"#,
                )?
                .query_row(
                    (
                        encode_eid(&bundle_id.source),
                        encode_creation_time(bundle_id.timestamp.creation_time),
                        as_i64(bundle_id.timestamp.sequence_number),
                        bundle_id.fragment_info.map_or(-1, |f| as_i64(f.offset)),
                        bundle_id.fragment_info.map_or(-1, |f| as_i64(f.total_len)),
                    ),
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            metadata::Metadata {
                                status: columns_to_bundle_status(row, 1, 2, 3)?,
                                storage_name: row.get(4)?,
                                hash: BASE64_STANDARD_NO_PAD
                                    .decode(row.get::<_, String>(5)?)
                                    .trace_expect("Failed to base64 decode hash")
                                    .into(),
                                received_at: row.get(6)?,
                            },
                        ))
                    },
                )
                .optional()?
            else {
                return Ok(None);
            };

            // Remove from unconfirmed set
            if trans
                .prepare_cached(r#"DELETE FROM unconfirmed_bundles WHERE bundle_id = ?1;"#)?
                .execute([bundle_id])?
                != 0
            {
                trans.commit()?;
            }

            Ok(Some(metadata))
        })
        .await
    }

    #[instrument(skip(self))]
    async fn get_bundle_status(
        &self,
        storage_name: &str,
    ) -> storage::Result<Option<metadata::BundleStatus>> {
        self
            .connection
            .lock()
            .await
            .prepare_cached(
                r#"SELECT status,ack_handle,wait_until FROM bundles WHERE storage_name = ?1 LIMIT 1;"#,
            )?
            .query_row(
                [storage_name],
                |row| columns_to_bundle_status(row,0,1,2),
            )
            .optional().map_err(Into::into)
    }

    #[instrument(skip(self))]
    async fn set_bundle_status(
        &self,
        storage_name: &str,
        status: &metadata::BundleStatus,
    ) -> storage::Result<()> {
        let (status, ack_handle, until) = bundle_status_to_parts(status);
        if !self
            .connection
            .lock().await
            .prepare_cached(
                r#"UPDATE bundles SET status = ?1, ack_handle = ?2, wait_until = ?3 WHERE storage_name = ?4;"#,
            )?
            .execute((status, ack_handle, until, storage_name))
            .map(|count| count != 0)?
        {
            Err(Error::NotFound.into())
        } else {
            Ok(())
        }
    }

    #[instrument(skip(self))]
    async fn begin_replace(&self, storage_name: &str, hash: &[u8]) -> storage::Result<()> {
        if !self
            .connection
            .lock()
            .await
            .prepare_cached(
                r#"INSERT INTO replacement_bundles (bundle_id, hash)
                SELECT id,?1 FROM bundles WHERE storage_name = ?2;"#,
            )?
            .execute((BASE64_STANDARD_NO_PAD.encode(hash), storage_name))
            .map(|count| count != 0)?
        {
            Err(Error::NotFound.into())
        } else {
            Ok(())
        }
    }

    #[instrument(skip(self))]
    async fn commit_replace(&self, storage_name: &str, hash: &[u8]) -> storage::Result<()> {
        let mut conn = self.connection.lock().await;
        let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        if complete_replace(&trans, storage_name, hash)?.is_none() {
            return Err(Error::NotFound.into());
        };

        // Commit the transaction
        trans.commit().map_err(Into::into)
    }

    #[instrument(skip(self, tx))]
    async fn get_waiting_bundles(
        &self,
        limit: time::OffsetDateTime,
        tx: storage::Sender,
    ) -> storage::Result<()> {
        self.sync_conn(move |conn| {
            unpack_bundles(
                conn.prepare_cached(
                    r#"SELECT 
                        bundles.id,
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
                        ack_handle,
                        block_num,
                        block_type,
                        block_flags,
                        block_crc_type,
                        data_offset,
                        data_len
                    FROM bundles
                    JOIN bundle_blocks ON bundle_blocks.bundle_id = bundles.id
                    WHERE status IN (?1,?2) AND unixepoch(wait_until) <= unixepoch(?3);"#,
                )?
                .query((
                    StatusCodes::ForwardAckPending as i64,
                    StatusCodes::Waiting as i64,
                    limit,
                ))?,
                &tx,
            )
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_unconfirmed_bundles(&self, tx: storage::Sender) -> storage::Result<()> {
        self.sync_conn(move |conn| {
            unpack_bundles(
                conn.prepare_cached(
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
                                wait_until,
                                ack_handle
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
                        JOIN bundle_blocks ON bundle_blocks.bundle_id = subset.id;"#,
                )?
                .query(())?,
                &tx,
            )
        })
        .await
    }

    #[instrument(skip(self, tx))]
    async fn poll_for_collection(
        &self,
        destination: bpv7::Eid,
        tx: storage::Sender,
    ) -> storage::Result<()> {
        self.sync_conn(move |conn| {
            unpack_bundles(
                conn.prepare_cached(
                    r#"SELECT 
                        bundles.id,
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
                        ack_handle,
                        block_num,
                        block_type,
                        block_flags,
                        block_crc_type,
                        data_offset,
                        data_len
                    FROM bundles
                    JOIN bundle_blocks ON bundle_blocks.bundle_id = bundles.id
                    WHERE status = ?1 AND destination = ?2;"#,
                )?
                .query((
                    StatusCodes::CollectionPending as i64,
                    encode_eid(&destination),
                ))?,
                &tx,
            )
        })
        .await
    }
}
