use super::*;
use hardy_bpa::{
    async_trait,
    metadata::{BundleMetadata, BundleStatus},
    storage,
};
use hardy_bpv7::prelude as bpv7;
use hardy_cbor as cbor;
use rusqlite::OptionalExtension;
use std::{cell::RefCell, collections::HashMap, path::PathBuf, sync::Arc};
use thiserror::Error;

thread_local! {
    static CONNECTION: RefCell<Option<rusqlite::Connection>> = const { RefCell::new(None) };
}

pub struct Storage {
    path: PathBuf,
    timeout: std::time::Duration,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("No such bundle")]
    NotFound,
}

#[derive(Debug)]
#[repr(i64)]
enum StatusCodes {
    DispatchPending = 1,
    ReassemblyPending = 2,
    Tombstone = 3,
}

impl From<i64> for StatusCodes {
    fn from(value: i64) -> Self {
        match value {
            1 => Self::DispatchPending,
            2 => Self::ReassemblyPending,
            3 => Self::Tombstone,
            _ => panic!("Invalid BundleStatus value {value}"),
        }
    }
}

impl From<StatusCodes> for i64 {
    fn from(value: StatusCodes) -> Self {
        value as i64
    }
}

fn bundle_status_to_parts(value: &BundleStatus) -> (i64, Option<time::OffsetDateTime>) {
    match value {
        BundleStatus::DispatchPending => (StatusCodes::DispatchPending.into(), None),
        BundleStatus::ReassemblyPending => (StatusCodes::ReassemblyPending.into(), None),
        BundleStatus::Tombstone(from) => (StatusCodes::Tombstone.into(), Some(*from)),
    }
}

fn columns_to_bundle_status(
    row: &rusqlite::Row,
    idx1: usize,
    idx2: usize,
) -> rusqlite::Result<BundleStatus> {
    match (
        row.get::<_, i64>(idx1)?.into(),
        row.get::<_, Option<time::OffsetDateTime>>(idx2)?,
    ) {
        (StatusCodes::DispatchPending, None) => Ok(BundleStatus::DispatchPending),
        (StatusCodes::ReassemblyPending, None) => Ok(BundleStatus::ReassemblyPending),
        (StatusCodes::Tombstone, Some(from)) => Ok(BundleStatus::Tombstone(from)),
        (v, d) => panic!("Invalid BundleStatus value combination {v:?}/{d:?}"),
    }
}

impl Storage {
    pub fn new(config: &Config, mut upgrade: bool) -> Self {
        // Compose DB name
        let file_path = config.db_dir.join("metadata.db");

        info!("Using database: {}", file_path.display());

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

        // Do an optimize check
        connection
            .execute_batch(r#"PRAGMA optimize=0x10002;"#)
            .trace_expect("Failed to set up metadata store database");

        // Mark all existing non-Tombstone bundles as unconfirmed
        connection
            .execute(
                r#"
            INSERT OR IGNORE INTO unconfirmed_bundles (bundle_id)
            SELECT id FROM bundles WHERE status != ?1;"#,
                [StatusCodes::Tombstone as i64],
            )
            .trace_expect("Failed to prepare metadata store database");

        Self {
            path: file_path,
            timeout: config.timeout,
        }
    }

    async fn pooled_connection<F, R>(&self, f: F) -> storage::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> storage::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let path = self.path.clone();
        let timeout = self.timeout;
        tokio::task::spawn_blocking(move || {
            CONNECTION.with_borrow_mut(|v| {
                if v.is_none() {
                    let conn = rusqlite::Connection::open_with_flags(
                        &path,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )?;
                    conn.busy_timeout(timeout)?;
                    *v = Some(conn);
                }
                f(v.as_mut().unwrap())
            })
        })
        .await
        .trace_expect("Failed to spawn blocking thread")
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

fn encode_hash(hash: &Option<Arc<[u8]>>) -> rusqlite::types::Value {
    match hash {
        Some(hash) => rusqlite::types::Value::Blob(hash.to_vec()),
        None => rusqlite::types::Value::Null,
    }
}

fn decode_hash(
    row: &rusqlite::Row,
    idx: impl rusqlite::RowIndex,
) -> rusqlite::Result<Option<Arc<[u8]>>> {
    match row.get_ref(idx)? {
        rusqlite::types::ValueRef::Blob(hash) => Ok(Some(hash.into())),
        rusqlite::types::ValueRef::Null => Ok(None),
        _ => panic!("hash encoded as unusual sqlite type"),
    }
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
    idx: impl rusqlite::RowIndex,
) -> rusqlite::Result<Option<bpv7::DtnTime>> {
    let timestamp = row.get(idx)?;
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

// Quick helper for type conversion
#[inline]
fn as_duration(v: i64) -> std::time::Duration {
    std::time::Duration::from_millis(v as u64)
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
           20: bundle_blocks.block_num,
           21: bundle_blocks.block_type,
           22: bundle_blocks.block_flags,
           23: bundle_blocks.block_crc_type,
           24: bundle_blocks.data_start,
           25: bundle_blocks.data_len,
           26: bundle_blocks.payload_offset,
           27: bundle_blocks.payload_len,
           28: bundle_blocks.bcb
    */

    while let Some(mut row) = rows.next()? {
        let bundle_id: i64 = row.get(0)?;
        let metadata = BundleMetadata {
            status: columns_to_bundle_status(row, 1, 19)?,
            storage_name: row.get(2)?,
            hash: decode_hash(row, 3)?,
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
            crc_type: as_u64(row.get(6)?).into(),
            destination: decode_eid(row, 8)?,
            report_to: decode_eid(row, 9)?,
            lifetime: as_duration(row.get(12)?),
            blocks: HashMap::new(),
            previous_node: match row.get_ref(15)? {
                rusqlite::types::ValueRef::Null => None,
                rusqlite::types::ValueRef::Blob(b) => Some(cbor::decode::parse(b)?),
                v => panic!("EID encoded as unusual sqlite type: {:?}", v),
            },
            age: row.get::<_, Option<i64>>(16)?.map(as_duration),
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
            let block_number = as_u64(row.get(20)?);
            let block = bpv7::Block {
                block_type: as_u64(row.get(21)?).into(),
                flags: as_u64(row.get(22)?).into(),
                crc_type: as_u64(row.get(23)?).into(),
                data_start: as_u64(row.get(24)?) as usize,
                data_len: as_u64(row.get(25)?) as usize,
                payload_offset: as_u64(row.get(26)?) as usize,
                payload_len: as_u64(row.get(27)?) as usize,
                bcb: row.get::<_, Option<i64>>(28)?.map(as_u64),
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

        if tx.blocking_send((metadata, bundle)).is_err() {
            break;
        }
    }
    Ok(())
}

#[async_trait]
impl storage::MetadataStorage for Storage {
    #[instrument(skip(self))]
    async fn load(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<(BundleMetadata, bpv7::Bundle)>> {
        let bundle_id = bundle_id.clone();
        self.pooled_connection(move |conn| {
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
                    block_num,
                    block_type,
                    block_flags,
                    block_crc_type,
                    data_start,
                    data_len,
                    payload_offset,
                    payload_len,
                    bcb
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
                bundle_id
                    .fragment_info
                    .as_ref()
                    .map_or(-1, |f| as_i64(f.offset)),
                bundle_id
                    .fragment_info
                    .as_ref()
                    .map_or(-1, |f| as_i64(f.total_len)),
            ))?;

            let Some(mut row) = rows.next()? else {
                return Ok(None);
            };

            let bundle_id: i64 = row.get(0)?;
            let metadata = BundleMetadata {
                status: columns_to_bundle_status(row, 1, 19)?,
                storage_name: row.get(2)?,
                hash: decode_hash(row, 3)?,
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
                crc_type: as_u64(row.get(6)?).into(),
                destination: decode_eid(row, 8)?,
                report_to: decode_eid(row, 9)?,
                lifetime: as_duration(row.get(12)?),
                blocks: HashMap::new(),
                previous_node: match row.get_ref(15)? {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Blob(b) => Some(cbor::decode::parse(b)?),
                    v => panic!("EID encoded as unusual sqlite type: {:?}", v),
                },
                age: row.get::<_, Option<i64>>(16)?.map(as_duration),
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
                let block_number = as_u64(row.get(20)?);
                let block = bpv7::Block {
                    block_type: as_u64(row.get(21)?).into(),
                    flags: as_u64(row.get(22)?).into(),
                    crc_type: as_u64(row.get(23)?).into(),
                    data_start: as_u64(row.get(24)?) as usize,
                    data_len: as_u64(row.get(25)?) as usize,
                    payload_offset: as_u64(row.get(26)?) as usize,
                    payload_len: as_u64(row.get(27)?) as usize,
                    bcb: row.get::<_, Option<i64>>(28)?.map(as_u64),
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
            Ok(Some((metadata, bundle)))
        })
        .await
    }

    #[instrument(skip(self))]
    async fn store(
        &self,
        metadata: &BundleMetadata,
        bundle: &bpv7::Bundle,
    ) -> storage::Result<bool> {
        let metadata = metadata.clone();
        let bundle = bundle.clone();
        self.pooled_connection(move |conn| {
            let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let (status, until) = bundle_status_to_parts(&metadata.status);

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
                    wait_until
                    )
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
                RETURNING id;"#,
                )?
                .query_row(
                    rusqlite::params!(
                        status,
                        &metadata.storage_name,
                        encode_hash(&metadata.hash),
                        as_i64(&bundle.flags),
                        as_i64(bundle.crc_type),
                        encode_eid(&bundle.id.source),
                        encode_eid(&bundle.destination),
                        encode_eid(&bundle.report_to),
                        encode_creation_time(bundle.id.timestamp.creation_time),
                        as_i64(bundle.id.timestamp.sequence_number),
                        as_i64(bundle.lifetime.as_millis() as u64),
                        bundle
                            .id
                            .fragment_info
                            .as_ref()
                            .map_or(-1, |f| as_i64(f.offset)),
                        bundle
                            .id
                            .fragment_info
                            .as_ref()
                            .map_or(-1, |f| as_i64(f.total_len)),
                        bundle.previous_node.as_ref().map(encode_eid),
                        bundle.age.map(|v| as_i64(v.as_millis() as u64)),
                        bundle.hop_count.as_ref().map(|h| as_i64(h.count)),
                        bundle.hop_count.as_ref().map(|h| as_i64(h.limit)),
                        until,
                    ),
                    |row| Ok(as_u64(row.get(0)?)),
                );

            let bundle_id = match bundle_id {
                Err(rusqlite::Error::SqliteFailure(e, _)) if e.extended_code == 2067 => {
                    return Ok(false);
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
                            data_start,
                            data_len,
                            payload_offset,
                            payload_len,
                            bcb)
                        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10);"#,
                )?;
                for (block_num, block) in &bundle.blocks {
                    block_stmt.execute((
                        bundle_id,
                        as_i64(block.block_type),
                        as_i64(*block_num),
                        as_i64(&block.flags),
                        as_i64(block.crc_type),
                        as_i64(block.data_start as u64),
                        as_i64(block.data_len as u64),
                        as_i64(block.payload_offset as u64),
                        as_i64(block.payload_len as u64),
                        block.bcb.map(as_i64),
                    ))?;
                }
            }

            // Commit transaction
            trans.commit().map(|_| true).map_err(Into::into)
        })
        .await
    }

    #[instrument(skip(self))]
    async fn remove(&self, bundle_id: &bpv7::BundleId) -> storage::Result<()> {
        let bundle_id = bundle_id.clone();
        self.pooled_connection(move |conn| {
            if !conn
                .prepare_cached(
                    r#"DELETE FROM bundles 
                    WHERE 
                        source = ?1 AND
                        creation_time = ?2 AND
                        creation_seq_num = ?3 AND
                        fragment_offset = ?4 AND 
                        fragment_total_len = ?5;"#,
                )?
                .execute((
                    encode_eid(&bundle_id.source),
                    encode_creation_time(bundle_id.timestamp.creation_time),
                    as_i64(bundle_id.timestamp.sequence_number),
                    bundle_id
                        .fragment_info
                        .as_ref()
                        .map_or(-1, |f| as_i64(f.offset)),
                    bundle_id
                        .fragment_info
                        .as_ref()
                        .map_or(-1, |f| as_i64(f.total_len)),
                ))
                .map(|count| count != 0)?
            {
                Err(Error::NotFound.into())
            } else {
                Ok(())
            }
        })
        .await
    }

    #[instrument(skip(self))]
    async fn confirm_exists(
        &self,
        bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<BundleMetadata>> {
        let bundle_id = bundle_id.clone();
        self.pooled_connection(move |conn| {
            let trans = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

            // Check if bundle exists
            let Some((bundle_id, metadata)) = trans
                .prepare_cached(
                    r#"SELECT 
                            id,
                            status,
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
                        bundle_id
                            .fragment_info
                            .as_ref()
                            .map_or(-1, |f| as_i64(f.offset)),
                        bundle_id
                            .fragment_info
                            .as_ref()
                            .map_or(-1, |f| as_i64(f.total_len)),
                    ),
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            BundleMetadata {
                                status: columns_to_bundle_status(row, 1, 2)?,
                                storage_name: row.get(3)?,
                                hash: decode_hash(row, 4)?,
                                received_at: row.get(5)?,
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
        bundle_id: &bpv7::BundleId,
    ) -> storage::Result<Option<BundleStatus>> {
        let bundle_id = bundle_id.clone();
        self.pooled_connection(move |conn| {
            conn.prepare_cached(
                r#"SELECT status,wait_until 
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
                    bundle_id
                        .fragment_info
                        .as_ref()
                        .map_or(-1, |f| as_i64(f.offset)),
                    bundle_id
                        .fragment_info
                        .as_ref()
                        .map_or(-1, |f| as_i64(f.total_len)),
                ),
                |row| columns_to_bundle_status(row, 0, 1),
            )
            .optional()
            .map_err(Into::into)
        })
        .await
    }

    #[instrument(skip(self))]
    async fn set_bundle_status(
        &self,
        bundle_id: &bpv7::BundleId,
        status: &BundleStatus,
    ) -> storage::Result<()> {
        let bundle_id = bundle_id.clone();
        let status = status.clone();
        self.pooled_connection(move |conn| {
            let (status_code, until) = bundle_status_to_parts(&status);

            let r = if let BundleStatus::Tombstone(_) = status {
                conn.prepare_cached(
                    r#"UPDATE bundles 
                    SET status = ?1, wait_until = ?2, storage_name = NULL, hash = NULL 
                    WHERE 
                        source = ?3 AND
                        creation_time = ?4 AND
                        creation_seq_num = ?5 AND
                        fragment_offset = ?6 AND 
                        fragment_total_len = ?7;"#,
                )?
                .execute((
                    status_code,
                    until,
                    encode_eid(&bundle_id.source),
                    encode_creation_time(bundle_id.timestamp.creation_time),
                    as_i64(bundle_id.timestamp.sequence_number),
                    bundle_id
                        .fragment_info
                        .as_ref()
                        .map_or(-1, |f| as_i64(f.offset)),
                    bundle_id
                        .fragment_info
                        .as_ref()
                        .map_or(-1, |f| as_i64(f.total_len)),
                ))
            } else {
                conn.prepare_cached(
                    r#"UPDATE bundles 
                    SET status = ?1, wait_until = ?2 
                    WHERE 
                        source = ?3 AND
                        creation_time = ?4 AND
                        creation_seq_num = ?5 AND
                        fragment_offset = ?6 AND 
                        fragment_total_len = ?7;"#,
                )?
                .execute((
                    status_code,
                    until,
                    encode_eid(&bundle_id.source),
                    encode_creation_time(bundle_id.timestamp.creation_time),
                    as_i64(bundle_id.timestamp.sequence_number),
                    bundle_id
                        .fragment_info
                        .as_ref()
                        .map_or(-1, |f| as_i64(f.offset)),
                    bundle_id
                        .fragment_info
                        .as_ref()
                        .map_or(-1, |f| as_i64(f.total_len)),
                ))
            };

            if !r.map(|count| count != 0)? {
                Err(Error::NotFound.into())
            } else {
                Ok(())
            }
        })
        .await
    }

    #[instrument(skip_all)]
    async fn get_unconfirmed_bundles(&self, tx: storage::Sender) -> storage::Result<()> {
        self.pooled_connection(move |conn| {
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
                            data_start,
                            data_len,
                            payload_offset,
                            payload_len,
                            bcb
                        FROM subset
                        JOIN bundle_blocks ON bundle_blocks.bundle_id = subset.id;"#,
                )?
                .query(())?,
                &tx,
            )
        })
        .await
    }
}
