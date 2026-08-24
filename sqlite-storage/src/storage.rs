use std::path::{Path, PathBuf};
use std::sync::Arc;

use hardy_bpa::{
    async_trait,
    bundle::{Bundle, BundleMetadata, BundleStatus},
    storage::{self, MetadataStorage},
    stream::Sender,
};

use rusqlite::OptionalExtension;
use trace_err::*;
use tracing::{debug, error, info, warn};

#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{migrate, pool::ConnectionPool};

// Filename of the SQLite database.
const DEFAULT_DB_NAME: &str = "metadata.db";

// Directory in which the database file is stored: the platform-specific
// cache directory for the project (e.g. `~/.cache/hardy-sqlite-storage` on
// Linux), or `/var/spool/<pkg>` on Unix when no project directory can be
// determined.
fn default_db_dir() -> PathBuf {
    directories::ProjectDirs::from("dtn", "Hardy", env!("CARGO_PKG_NAME")).map_or_else(
        || cfg_select! {
            unix => Path::new("/var/spool").join(env!("CARGO_PKG_NAME")),
            windows => std::env::current_exe().expect("Failed to get current executable path").join(env!("CARGO_PKG_NAME")),
            _ => compile_error!("No idea how to determine default sqlite metadata store directory for target platform"),
        },
        |project_dirs| project_dirs.cache_dir().into(),
    )
}

/// SQLite-backed implementation of [`MetadataStorage`](storage::MetadataStorage).
///
/// Manages a pool of read connections and a single serialized write lock to
/// avoid SQLite busy errors. Bundle metadata is stored as JSON blobs alongside
/// typed status columns for efficient status-based queries.
pub struct SqliteStorage {
    pool: Arc<ConnectionPool>,
}

impl SqliteStorage {
    /// Opens or creates the SQLite database and runs schema migrations.
    ///
    /// `None` applies the backend's own default: the platform cache
    /// directory, and `metadata.db`. If the database file does not exist it
    /// is created and `upgrade` is forced to `true`. When `upgrade` is
    /// `true`, pending schema migrations are applied.
    pub fn new(db_dir: Option<PathBuf>, db_name: Option<String>, mut upgrade: bool) -> Self {
        let db_dir = db_dir.unwrap_or_else(default_db_dir);
        let db_name = db_name.as_deref().unwrap_or(DEFAULT_DB_NAME);

        // Ensure directory exists
        std::fs::create_dir_all(&db_dir).trace_expect(&format!(
            "Failed to create metadata store directory {}",
            db_dir.display()
        ));

        // Compose DB name
        let path = db_dir.join(db_name);

        info!("Using database: {}", path.display());

        // Attempt to open existing database first
        let mut connection = match rusqlite::Connection::open_with_flags(
            &path,
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
                    &path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
            }
            r => r,
        }
        .trace_expect("Failed to open metadata store database");

        // connection
        //     .busy_timeout(std::time::Duration::ZERO)
        //     .trace_expect("Failed to set timeout");

        // journal_mode cannot be changed inside a transaction (migrations run
        // in one), so WAL is applied here, before migrate(), not in the schema.
        // synchronous is a per-connection pragma; NORMAL under WAL fsyncs at
        // checkpoint rather than per commit (see new_connection for the
        // durability rationale).
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA foreign_keys = ON;
                PRAGMA optimize = 0x10002;",
            )
            .trace_expect("Failed to prepare metadata store database");

        rusqlite::vtab::array::load_module(&connection).trace_expect("Failed to load array module");

        // Migrate the database to the latest schema
        migrate::migrate(&mut connection, upgrade)
            .trace_expect("Failed to migrate metadata store database");

        Self {
            pool: Arc::new(ConnectionPool::new(path, connection)),
        }
    }

    async fn read<F, R>(&self, f: F) -> storage::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> storage::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let mut conn = self.pool.get(None).await;
        let r = f(&mut conn);
        self.pool.put(conn);
        r
    }

    async fn write<F, R>(&self, f: F) -> storage::Result<R>
    where
        F: FnOnce(&mut rusqlite::Connection) -> storage::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let guard = self.pool.write_lock.lock().await;
        let mut conn = self.pool.get(Some(&guard)).await;
        let r = f(&mut conn);
        drop(guard);
        self.pool.put(conn);
        r
    }
}

// status_code layout:
//
// 0 = New
// 1 = Waiting
// 2 = ForwardPending(peer, queue)
// 3 = AduFragment(timestamp, seq, source)
// 4 = Dispatching
// 5 = WaitingForService(source)
// 6 = ForwardAckPending(peer)
fn from_status(status: &BundleStatus) -> (i64, Option<i64>, Option<i64>, Option<String>) {
    match status {
        BundleStatus::New => (0, None, None, None),
        BundleStatus::Waiting => (1, None, None, None),
        BundleStatus::ForwardPending { peer, queue } => {
            (2, Some(*peer as i64), queue.map(|q| q as i64), None)
        }
        BundleStatus::AduFragment { source, timestamp } => (
            3,
            Some(
                timestamp
                    .creation_time()
                    .map_or(0i64, |t| t.millisecs() as i64),
            ),
            Some(timestamp.sequence_number() as i64),
            Some(source.to_string()),
        ),
        BundleStatus::Dispatching => (4, None, None, None),
        BundleStatus::WaitingForService { service } => (5, None, None, Some(service.to_string())),
        BundleStatus::ForwardAckPending { peer } => (6, Some(i64::from(*peer)), None, None),
    }
}

fn to_status(
    code: i64,
    param1: Option<i64>,
    param2: Option<i64>,
    param3: Option<String>,
) -> Option<BundleStatus> {
    match code {
        0 => Some(BundleStatus::New),
        1 => Some(BundleStatus::Waiting),
        2 => Some(BundleStatus::ForwardPending {
            peer: param1? as u32,
            queue: param2.map(|q| q as u32),
        }),
        3 => {
            let source: hardy_bpv7::eid::Eid = param3?.parse().ok()?;
            let creation_time = param1
                .filter(|&ms| ms != 0)
                .map(|ms| hardy_bpv7::dtn_time::DtnTime::new(ms as u64));
            let sequence_number = param2? as u64;
            let timestamp = hardy_bpv7::creation_timestamp::CreationTimestamp::from_parts(
                creation_time,
                sequence_number,
            );
            Some(BundleStatus::AduFragment { source, timestamp })
        }
        4 => Some(BundleStatus::Dispatching),
        5 => Some(BundleStatus::WaitingForService {
            service: param3?.parse().ok()?,
        }),
        6 => Some(BundleStatus::ForwardAckPending {
            peer: u32::try_from(param1?).ok()?,
        }),
        _ => None,
    }
}

#[async_trait]
impl MetadataStorage for SqliteStorage {
    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    async fn get(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<Option<Bundle>> {
        let id = serde_json::to_vec(bundle_id)?;
        let Some((bundle, status_code, p1, p2, p3)) = self
            .read(move |conn| {
                conn
                    .prepare_cached(
                        "SELECT bundle, status_code, status_param1, status_param2, status_param3 FROM bundles WHERE bundle_id = ?1 AND bundle IS NOT NULL LIMIT 1",
                    )?
                    .query_row((&id,), |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    })
                    .optional().map_err(Into::into)
            })
            .await?
        else {
            return Ok(None);
        };

        let mut bundle: Bundle = serde_json::from_slice(&bundle)?;
        if let Some(status) = to_status(status_code, p1, p2, p3) {
            bundle.metadata.status = status;
            Ok(Some(bundle))
        } else {
            warn!("Failed to unpack metadata status: code = {status_code}");
            Ok(None)
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    async fn insert(&self, bundle: &Bundle) -> storage::Result<bool> {
        let expiry = bundle.expiry();
        let received_at = bundle.metadata.read_only.received_at;
        let (status_code, status_param1, status_param2, status_param3) =
            from_status(&bundle.metadata.status);
        let id = serde_json::to_vec(&bundle.bundle.id)?;
        let bundle = serde_json::to_vec(bundle)?;
        self.write(move |conn| {
            // Insert bundle
            conn.prepare_cached(
                "INSERT OR IGNORE INTO bundles (bundle_id,bundle,expiry,received_at,status_code,status_param1,status_param2,status_param3) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?
            .execute((id,bundle,expiry,received_at,status_code,status_param1,status_param2,status_param3))
            .map(|c| c == 1)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    async fn replace(&self, bundle: &Bundle) -> storage::Result<()> {
        let expiry = bundle.expiry();
        let received_at = bundle.metadata.read_only.received_at;
        let (status_code, status_param1, status_param2, status_param3) =
            from_status(&bundle.metadata.status);
        let id = serde_json::to_vec(&bundle.bundle.id)?;
        let bundle = serde_json::to_vec(bundle)?;
        if self
            .write(move |conn| {
                // Update bundle
                conn.prepare_cached(
                    "UPDATE bundles SET bundle = ?2, expiry = ?3, received_at = ?4, status_code = ?5, status_param1 = ?6, status_param2 = ?7, status_param3 = ?8 WHERE bundle_id = ?1",
                )?
                .execute((id,bundle,expiry,received_at,status_code,status_param1,status_param2,status_param3))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            error!("Failed to replace bundle!");
        }
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.bundle.id)))]
    async fn update_status(&self, bundle: &Bundle) -> storage::Result<()> {
        let (status_code, status_param1, status_param2, status_param3) =
            from_status(&bundle.metadata.status);
        let id = serde_json::to_vec(&bundle.bundle.id)?;
        if self
            .write(move |conn| {
                conn.prepare_cached(
                    "UPDATE bundles SET status_code = ?2, status_param1 = ?3, status_param2 = ?4, status_param3 = ?5 WHERE bundle_id = ?1 AND bundle IS NOT NULL",
                )?
                .execute((id, status_code, status_param1, status_param2, status_param3))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            // Delete is terminal: the bundle was removed between the
            // caller's read and this write, and the update quietly loses
            debug!("Status update for a deleted bundle, ignored");
        }
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    async fn swap_status(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        expected: &BundleStatus,
        status: &BundleStatus,
    ) -> storage::Result<bool> {
        let (expected_code, expected_param1, expected_param2, expected_param3) =
            from_status(expected);
        let (status_code, status_param1, status_param2, status_param3) = from_status(status);
        let id = serde_json::to_vec(bundle_id)?;
        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET status_code = ?2, status_param1 = ?3, status_param2 = ?4, status_param3 = ?5 \
                 WHERE bundle_id = ?1 AND status_code = ?6 AND status_param1 IS ?7 AND status_param2 IS ?8 AND status_param3 IS ?9",
            )?
            .execute((
                id,
                status_code,
                status_param1,
                status_param2,
                status_param3,
                expected_code,
                expected_param1,
                expected_param2,
                expected_param3,
            ))
            .map_err(Into::into)
        })
        .await
        .map(|rows| rows == 1)
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    async fn tombstone_if(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
        expected: &BundleStatus,
    ) -> storage::Result<bool> {
        let (expected_code, expected_param1, expected_param2, expected_param3) =
            from_status(expected);
        let id = serde_json::to_vec(bundle_id)?;
        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET bundle = NULL, status_code = NULL, status_param1 = NULL, status_param2 = NULL, status_param3 = NULL \
                 WHERE bundle_id = ?1 AND status_code = ?2 AND status_param1 IS ?3 AND status_param2 IS ?4 AND status_param3 IS ?5",
            )?
            .execute((
                id,
                expected_code,
                expected_param1,
                expected_param2,
                expected_param3,
            ))
            .map_err(Into::into)
        })
        .await
        .map(|rows| rows == 1)
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    async fn tombstone(&self, bundle_id: &hardy_bpv7::bundle::Id) -> storage::Result<()> {
        let id = serde_json::to_vec(bundle_id)?;
        if self
            .write(move |conn| {
                conn.prepare_cached(
                    "UPDATE bundles SET bundle = NULL, status_code = NULL, status_param1 = NULL, status_param2 = NULL, status_param3 = NULL WHERE bundle_id = ?1",
                )?
                .execute((id,))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            debug!("Tombstone for a missing bundle, ignored");
        }
        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn start_recovery(&self) {
        self
            .write(move |conn| {
                conn.execute_batch("INSERT OR IGNORE INTO unconfirmed_bundles (id) SELECT id FROM bundles WHERE bundle IS NOT NULL")
                .map_err(Into::into)
            })
            .await.unwrap_or_else(|e|
        {
            error!("Failed to mark unconfirmed bundles!: {e}");
        })
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle_id)))]
    async fn confirm_exists(
        &self,
        bundle_id: &hardy_bpv7::bundle::Id,
    ) -> storage::Result<Option<BundleMetadata>> {
        let id = serde_json::to_vec(bundle_id)?;
        let Some((bundle, status_code, p1, p2, p3))  = self
            .write(move |conn| {
                conn.prepare_cached(
                    "DELETE FROM unconfirmed_bundles WHERE id = (SELECT id FROM bundles WHERE bundle_id = ?1)",
                )?
                .execute((&id,))?;

                conn.prepare_cached(
                    "SELECT bundle, status_code, status_param1, status_param2, status_param3 FROM bundles WHERE bundle_id = ?1 LIMIT 1",
                )?
                .query_row((id,), |row| {
                     Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                })
                .optional()
                .map_err(Into::into)
            })
            .await? else {
            return Ok(None);
        };

        match serde_json::from_slice::<Bundle>(&bundle) {
            Ok(mut bundle) => {
                if let Some(status) = to_status(status_code, p1, p2, p3) {
                    bundle.metadata.status = status;
                    Ok(Some(bundle.metadata))
                } else {
                    error!("Failed to unpack metadata status: code = {status_code}");
                    self.tombstone(bundle_id).await.map(|_| None)
                }
            }
            Err(e) => {
                warn!("Garbage bundle found in metadata: {e}");
                self.tombstone(bundle_id).await.map(|_| None)
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn remove_unconfirmed(&self, stream: &dyn Sender<Bundle>) -> storage::Result<()> {
        loop {
            let bundles = self
                .write(move |conn| {
                    let trans =
                        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                    let ids = trans
                        .prepare_cached(
                            "DELETE FROM unconfirmed_bundles
                            WHERE id IN (SELECT id FROM unconfirmed_bundles LIMIT 64)
                            RETURNING id",
                        )?
                        .query_map([], |row| row.get(0))?
                        .collect::<Result<Vec<i64>, _>>()?;

                    if ids.is_empty() {
                        return Ok(Vec::new());
                    }

                    let id_values = std::rc::Rc::new(
                        ids.into_iter()
                            .map(rusqlite::types::Value::from)
                            .collect::<Vec<_>>(),
                    );

                    // Snapshot the blobs before tombstoning: RETURNING on an
                    // UPDATE reports the new (nulled) column values, so the
                    // bundles to emit must be read first.
                    let bundles = trans
                        .prepare_cached(
                            "SELECT bundle FROM bundles WHERE id IN rarray(?1) AND bundle IS NOT NULL",
                        )?
                        .query_map([id_values.clone()], |row| row.get(0))?
                        .collect::<Result<Vec<Vec<u8>>, _>>()?;

                    trans
                        .prepare_cached(
                            "UPDATE bundles SET bundle = NULL, status_code = NULL, status_param1 = NULL, status_param2 = NULL, status_param3 = NULL WHERE id IN rarray(?1) AND bundle IS NOT NULL",
                        )?
                        .execute([id_values])?;

                    trans.commit()?;

                    Ok(bundles)
                })
                .await?;

            if bundles.is_empty() {
                return Ok(());
            }

            for bundle in bundles {
                match serde_json::from_slice(&bundle) {
                    Ok(bundle) => {
                        if stream.send(bundle).await.is_err() {
                            // The other end is shutting down - get out
                            return Ok(());
                        }
                    }
                    Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn reset_peer_queue(&self, peer: u32) -> storage::Result<u64> {
        // Ensure status codes match
        debug_assert!(
            from_status(&BundleStatus::Waiting).0 == 1,
            "Status code mismatch"
        );
        debug_assert!(
            from_status(&BundleStatus::ForwardPending {
                peer,
                queue: Some(0)
            }) == (2, Some(peer as i64), Some(0), None),
            "Status code mismatch"
        );

        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET status_code = 1, status_param1 = NULL, status_param2 = NULL WHERE status_code = 2 AND status_param1 = ?1",
            )?
            .execute((Some(peer),))
            .map(|c| c as u64)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn reset_peer_ack_pending(&self, peer: u32) -> storage::Result<u64> {
        // Ensure status codes match
        debug_assert!(
            from_status(&BundleStatus::Waiting).0 == 1,
            "Status code mismatch"
        );
        debug_assert!(
            from_status(&BundleStatus::ForwardAckPending { peer })
                == (6, Some(peer as i64), None, None),
            "Status code mismatch"
        );

        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET status_code = 1, status_param1 = NULL WHERE status_code = 6 AND status_param1 = ?1",
            )?
            .execute((Some(peer),))
            .map(|c| c as u64)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>, limit: usize) -> storage::Result<()> {
        debug_assert!(
            from_status(&BundleStatus::New).0 == 0,
            "Status code mismatch"
        ); // Ensure status codes match

        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle, status_code, status_param1, status_param2, status_param3 FROM bundles
                        WHERE bundle IS NOT NULL AND status_code != 0
                        ORDER BY expiry ASC
                        LIMIT ?1",
                )?
                .query_map((limit as isize,), |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for (bundle, status_code, p1, p2, p3) in bundles {
            match serde_json::from_slice::<Bundle>(&bundle) {
                Ok(mut bundle) => {
                    if let Some(status) = to_status(status_code, p1, p2, p3) {
                        bundle.metadata.status = status;
                        if stream.send(bundle).await.is_err() {
                            // The other end is shutting down - get out
                            break;
                        }
                    } else {
                        warn!("Failed to unpack metadata status: code = {status_code}");
                    }
                }
                Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> storage::Result<()> {
        debug_assert!(
            from_status(&BundleStatus::Waiting).0 == 1,
            "Status code mismatch"
        ); // Ensure status codes match

        // Refresh the waiting queue
        self.write(move |conn| {
            conn.execute_batch(
                "INSERT OR IGNORE INTO waiting_queue (id,received_at) SELECT id,received_at FROM bundles WHERE status_code = 1",
            )
            .map_err(Into::into)
        }).await?;

        loop {
            let bundles = self
                .write(move |conn| {
                    let trans =
                        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

                    let ids = trans
                        .prepare_cached(
                            "DELETE FROM waiting_queue 
                            WHERE id IN (SELECT id FROM waiting_queue ORDER BY received_at ASC LIMIT 64)
                            RETURNING id",
                        )?
                        .query_map([], |row| row.get(0))?
                        .collect::<Result<Vec<i64>, _>>()?;

                    if ids.is_empty() {
                        return Ok(Vec::new()); // No bundles to process
                    }

                    let id_values = std::rc::Rc::new(
                        ids.into_iter()
                            .map(rusqlite::types::Value::from)
                            .collect::<Vec<_>>(),
                    );

                    let bundles = trans
                        .prepare_cached("SELECT bundle FROM bundles WHERE id IN rarray(?1) AND bundle IS NOT NULL ORDER BY received_at ASC")?
                        .query_map([id_values], |row| row.get::<_, Vec<u8>>(0))?
                        .collect::<Result<Vec<Vec<u8>>, _>>()?;

                    trans.commit()?;

                    Ok(bundles)
                })
                .await?;

            if bundles.is_empty() {
                return Ok(());
            }

            for bundle in bundles {
                match serde_json::from_slice::<Bundle>(&bundle) {
                    Ok(mut bundle) => {
                        bundle.metadata.status = BundleStatus::Waiting;
                        if stream.send(bundle).await.is_err() {
                            // The other end is shutting down - get out
                            return Ok(());
                        }
                    }
                    Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
                }
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    async fn poll_service_waiting(
        &self,
        source: hardy_bpv7::eid::Eid,
        stream: &dyn Sender<Bundle>,
    ) -> storage::Result<()> {
        debug_assert!(
            from_status(&BundleStatus::WaitingForService {
                service: source.clone()
            })
            .0 == 5,
            "Status code mismatch"
        ); // Ensure status codes match

        let source_str = source.to_string();
        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles
                        WHERE bundle IS NOT NULL AND status_code = 5 AND status_param3 = ?1
                        ORDER BY received_at ASC",
                )?
                .query_map((source_str,), |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match serde_json::from_slice::<Bundle>(&bundle) {
                Ok(mut bundle) => {
                    bundle.metadata.status = BundleStatus::WaitingForService {
                        service: source.clone(),
                    };
                    if stream.send(bundle).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    async fn poll_adu_fragments(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
    ) -> storage::Result<()> {
        let (status_code, status_param1, status_param2, status_param3) = from_status(status);

        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles
                        WHERE bundle IS NOT NULL AND status_code = ?1 AND status_param1 IS ?2 AND status_param2 IS ?3 AND status_param3 IS ?4
                        ORDER BY received_at ASC",
                )?
                .query_map((status_code, status_param1, status_param2,status_param3), |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match serde_json::from_slice::<Bundle>(&bundle) {
                Ok(mut bundle) => {
                    bundle.metadata.status = status.clone();
                    if stream.send(bundle).await.is_err() {
                        // The other end is shutting down - get out
                        break;
                    }
                }
                Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
            }
        }

        Ok(())
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    async fn poll_pending(
        &self,
        stream: &dyn Sender<Bundle>,
        status: &BundleStatus,
        limit: usize,
    ) -> storage::Result<()> {
        let (status_code, status_param1, status_param2, status_param3) = from_status(status);

        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles
                        WHERE bundle IS NOT NULL AND status_code = ?1 AND status_param1 IS ?2 AND status_param2 IS ?3 AND status_param3 IS ?4
                        ORDER BY received_at ASC
                        LIMIT ?5",
                )?
                .query_map((status_code, status_param1, status_param2,status_param3, limit as isize), |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match serde_json::from_slice::<Bundle>(&bundle) {
                Ok(mut bundle) => {
                    bundle.metadata.status = status.clone();
                    if stream.send(bundle).await.is_err() {
                        // The other end is shutting down - get out
                        break;
                    }
                }
                Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
            }
        }

        Ok(())
    }
}
