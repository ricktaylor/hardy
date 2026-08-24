use std::path::{Path, PathBuf};
use std::sync::Arc;

use hardy_bpa::{
    async_trait,
    bundle::{Bundle, BundleStatus, StoredBundle, StoredBundleRef},
    storage::{self, ConfirmResponse, MetadataStorage},
    stream::Sender,
};
use hardy_bpv7::eid::Eid;

use rusqlite::OptionalExtension;
use time::UtcOffset;
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
// 5 = WaitingForService(service)
// 6 = ForwardAckPending(peer)
// 7 = DispatchPending
// 8 = DeliverPending(service)
// 9 = DeliveryAckPending(service)
fn from_status(status: &BundleStatus) -> (i64, Option<i64>, Option<i64>, Option<String>) {
    match status {
        BundleStatus::New => (0, None, None, None),
        BundleStatus::Waiting => (1, None, None, None),
        BundleStatus::ForwardPending { peer, queue } => {
            (2, Some(*peer as i64), Some(*queue as i64), None)
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
        BundleStatus::DispatchPending => (7, None, None, None),
        BundleStatus::DeliverPending { service } => (8, None, None, Some(service.to_string())),
        BundleStatus::DeliveryAckPending { service } => (9, None, None, Some(service.to_string())),
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
            peer: u32::try_from(param1?).ok()?,
            queue: u32::try_from(param2?).ok()?,
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
        7 => Some(BundleStatus::DispatchPending),
        8 => Some(BundleStatus::DeliverPending {
            service: param3?.parse().ok()?,
        }),
        9 => Some(BundleStatus::DeliveryAckPending {
            service: param3?.parse().ok()?,
        }),
        _ => None,
    }
}

// The poll_pending page shape, hoisted so the plan pin test below EXPLAINs
// the exact production SQL.
const POLL_PENDING_SQL: &str = "SELECT bundle FROM bundles
    WHERE bundle IS NOT NULL AND status_code = ?1 AND status_param1 IS ?2 AND status_param2 IS ?3 AND status_param3 IS ?4
    ORDER BY received_at ASC
    LIMIT ?5";

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

        let stored: StoredBundle = serde_json::from_slice(&bundle)?;
        if let Some(status) = to_status(status_code, p1, p2, p3) {
            Ok(Some(stored.into_bundle(status)))
        } else {
            warn!("Failed to unpack metadata status: code = {status_code}");
            Ok(None)
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    async fn insert(&self, bundle: &Bundle) -> storage::Result<bool> {
        // Normalized to UTC so the TEXT `expiry` column's lexicographic
        // order is chronological — rusqlite stores the value's own offset,
        // and `poll_expiry`'s keyset cursor depends on a uniform one.
        let expiry = bundle.expiry().to_offset(UtcOffset::UTC);
        let received_at = bundle.metadata.received_at();
        let (status_code, status_param1, status_param2, status_param3) =
            from_status(&bundle.status);
        let id = serde_json::to_vec(bundle.id())?;
        let bundle = serde_json::to_vec(&StoredBundleRef::from(bundle))?;
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

    #[cfg_attr(feature = "instrument", instrument(skip_all,fields(bundle.id = %bundle.id())))]
    async fn replace(&self, bundle: &Bundle) -> storage::Result<()> {
        // UTC-normalized for the same reason as `insert`.
        let expiry = bundle.expiry().to_offset(UtcOffset::UTC);
        let received_at = bundle.metadata.received_at();
        let (status_code, status_param1, status_param2, status_param3) =
            from_status(&bundle.status);
        let id = serde_json::to_vec(bundle.id())?;
        let bundle = serde_json::to_vec(&StoredBundleRef::from(bundle))?;
        if self
            .write(move |conn| {
                // `bundle IS NOT NULL` keeps a tombstone a tombstone: the row
                // survives deletion with its columns nulled, so an unqualified
                // UPDATE would write the blob and status straight back in and
                // resurrect the bundle. Matching no row is the defined outcome
                // for a write that lost its race, not an error.
                conn.prepare_cached(
                    "UPDATE bundles SET bundle = ?2, expiry = ?3, received_at = ?4, status_code = ?5, status_param1 = ?6, status_param2 = ?7, status_param3 = ?8 WHERE bundle_id = ?1 AND bundle IS NOT NULL",
                )?
                .execute((id,bundle,expiry,received_at,status_code,status_param1,status_param2,status_param3))
                .map_err(Into::into)
            })
            .await?
            != 1
        {
            debug!("Replace for a missing or tombstoned bundle, ignored");
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
    ) -> storage::Result<Option<ConfirmResponse>> {
        let id = serde_json::to_vec(bundle_id)?;
        let Some((bundle, status_code, p1, p2, p3))  = self
            .write(move |conn| {
                conn.prepare_cached(
                    "DELETE FROM unconfirmed_bundles WHERE id = (SELECT id FROM bundles WHERE bundle_id = ?1)",
                )?
                .execute((&id,))?;

                conn.prepare_cached(
                    "SELECT bundle, status_code, status_param1, status_param2, status_param3 FROM bundles WHERE bundle_id = ?1 AND bundle IS NOT NULL LIMIT 1",
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

        match serde_json::from_slice::<StoredBundle>(&bundle) {
            Ok(stored) => {
                if let Some(status) = to_status(status_code, p1, p2, p3) {
                    let bundle = stored.into_bundle(status);
                    Ok(Some((bundle.metadata, bundle.status)))
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
                match serde_json::from_slice::<StoredBundle>(&bundle) {
                    // The removal above NULLed the typed status columns, so
                    // the record's status is gone; the consumer only reports
                    // the unconfirmed orphan, and `New` — a record ingress
                    // never finished committing — is exactly what it was.
                    Ok(stored) => {
                        if stream
                            .send(stored.into_bundle(BundleStatus::New))
                            .await
                            .is_err()
                        {
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
        // Both statuses bind through the codec: the values in the SQL are
        // from_status's own output, so codec/SQL drift is unrepresentable.
        let (from_code, from_p1, _, _) =
            from_status(&BundleStatus::ForwardPending { peer, queue: 0 });
        let (to_code, to_p1, to_p2, _) = from_status(&BundleStatus::Waiting);

        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET status_code = ?1, status_param1 = ?2, status_param2 = ?3 WHERE status_code = ?4 AND status_param1 = ?5",
            )?
            .execute((to_code, to_p1, to_p2, from_code, from_p1))
            .map(|c| c as u64)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn reset_peer_ack_pending(&self, peer: u32) -> storage::Result<u64> {
        let (from_code, from_p1, _, _) = from_status(&BundleStatus::ForwardAckPending { peer });
        let (to_code, to_p1, _, _) = from_status(&BundleStatus::Waiting);

        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET status_code = ?1, status_param1 = ?2 WHERE status_code = ?3 AND status_param1 = ?4",
            )?
            .execute((to_code, to_p1, from_code, from_p1))
            .map(|c| c as u64)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    async fn reset_service_queue(&self, service: &Eid) -> storage::Result<u64> {
        // The service EID string (param3) is the same in both statuses, so
        // only the code changes; all three values bind through the codec.
        let (from_code, _, _, from_p3) = from_status(&BundleStatus::DeliverPending {
            service: service.clone(),
        });
        let (to_code, _, _, _) = from_status(&BundleStatus::WaitingForService {
            service: service.clone(),
        });

        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE bundles SET status_code = ?1 WHERE status_code = ?2 AND status_param3 = ?3",
            )?
            .execute((to_code, from_code, from_p3))
            .map(|c| c as u64)
            .map_err(Into::into)
        })
        .await
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self, stream)))]
    async fn poll_expiry(&self, stream: &dyn Sender<Bundle>) -> storage::Result<()> {
        let (new_code, _, _, _) = from_status(&BundleStatus::New);

        // Keyset pages: the consumer closes the stream once it has what it
        // needs, so each page is fetched only if the previous one was
        // consumed whole. The `(expiry, rowid) > (?1, ?2)` cursor and the
        // ORDER BY compare the TEXT `expiry` column lexicographically,
        // which is chronological order because every write site normalizes
        // the value to UTC before binding (rusqlite encodes the value's
        // own offset, so a mixed-offset table would sort wrong) — see
        // `insert`. This keeps both the cursor and the sort on
        // `idx_bundles_expiry`; an expression like `datetime(expiry)`
        // would forfeit the index and truncate sub-second precision.
        const PAGE_SIZE: usize = 64;
        let mut cursor: Option<(String, i64)> = None;
        loop {
            let page_cursor = cursor.clone();
            let bundles = self
                .read(move |conn| {
                    let (expiry, rowid) = page_cursor
                        .unwrap_or_else(|| (String::new(), 0));
                    conn.prepare_cached(
                        "SELECT rowid, expiry, bundle, status_code, status_param1, status_param2, status_param3 FROM bundles
                            WHERE bundle IS NOT NULL AND status_code != ?4 AND (expiry, rowid) > (?1, ?2)
                            ORDER BY expiry ASC, rowid ASC
                            LIMIT ?3",
                    )?
                    .query_map((expiry, rowid, PAGE_SIZE as isize, new_code), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                            row.get::<_, Option<i64>>(5)?,
                            row.get::<_, Option<String>>(6)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(Into::into)
                })
                .await?;

            let full_page = bundles.len() == PAGE_SIZE;
            for (rowid, expiry, bundle, status_code, p1, p2, p3) in bundles {
                cursor = Some((expiry, rowid));
                match serde_json::from_slice::<StoredBundle>(&bundle) {
                    Ok(stored) => {
                        if let Some(status) = to_status(status_code, p1, p2, p3) {
                            if stream.send(stored.into_bundle(status)).await.is_err() {
                                // The other end is shutting down - get out
                                return Ok(());
                            }
                        } else {
                            warn!("Failed to unpack metadata status: code = {status_code}");
                        }
                    }
                    Err(e) => warn!("Garbage bundle found and dropped from metadata: {e}"),
                }
            }
            if !full_page {
                return Ok(());
            }
        }
    }

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn poll_waiting(&self, stream: &dyn Sender<Bundle>) -> storage::Result<()> {
        let (waiting_code, _, _, _) = from_status(&BundleStatus::Waiting);

        // Refresh the waiting queue
        self.write(move |conn| {
            conn.prepare_cached(
                "INSERT OR IGNORE INTO waiting_queue (id,received_at) SELECT id,received_at FROM bundles WHERE status_code = ?1",
            )?
            .execute((waiting_code,))
            .map(|_| ())
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
                match serde_json::from_slice::<StoredBundle>(&bundle) {
                    Ok(stored) => {
                        if stream
                            .send(stored.into_bundle(BundleStatus::Waiting))
                            .await
                            .is_err()
                        {
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
        let (code, _, _, p3) = from_status(&BundleStatus::WaitingForService {
            service: source.clone(),
        });
        let bundles = self
            .read(move |conn| {
                conn.prepare_cached(
                    "SELECT bundle FROM bundles
                        WHERE bundle IS NOT NULL AND status_code = ?1 AND status_param3 = ?2
                        ORDER BY received_at ASC",
                )?
                .query_map((code, p3), |row| row.get::<_, Vec<u8>>(0))?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match serde_json::from_slice::<StoredBundle>(&bundle) {
                Ok(stored) => {
                    let bundle = stored.into_bundle(BundleStatus::WaitingForService {
                        service: source.clone(),
                    });
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
            match serde_json::from_slice::<StoredBundle>(&bundle) {
                Ok(stored) => {
                    if stream
                        .send(stored.into_bundle(status.clone()))
                        .await
                        .is_err()
                    {
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
                conn.prepare_cached(POLL_PENDING_SQL)?
                    .query_map(
                        (
                            status_code,
                            status_param1,
                            status_param2,
                            status_param3,
                            limit as isize,
                        ),
                        |row| row.get::<_, Vec<u8>>(0),
                    )?
                    .collect::<Result<Vec<Vec<u8>>, _>>()
                    .map_err(Into::into)
            })
            .await?;

        for bundle in bundles {
            match serde_json::from_slice::<StoredBundle>(&bundle) {
                Ok(stored) => {
                    if stream
                        .send(stored.into_bundle(status.clone()))
                        .await
                        .is_err()
                    {
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

#[cfg(test)]
mod tests {
    use hardy_bpa::bundle::BundleStatus;

    use super::{POLL_PENDING_SQL, SqliteStorage, from_status, to_status};

    // The poll_pending page query must be served in index order: a plan
    // with a TEMP B-TREE re-sorts the whole backlog per drain page, an
    // O(B^2) drain in exactly the overload regime the poller exists for
    // (see schemas/02_poll_pending_index.sql for the index rationale).
    #[test]
    fn poll_pending_plan_has_no_temp_btree() {
        let dir = tempfile::tempdir().unwrap();
        let _storage =
            SqliteStorage::new(Some(dir.path().to_path_buf()), Some("test.db".into()), true);

        let conn = rusqlite::Connection::open(dir.path().join("test.db")).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {POLL_PENDING_SQL}"))
            .unwrap()
            .query_map(
                rusqlite::params![8i64, None::<i64>, None::<i64>, Some("ipn:60.3"), 16isize],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            !plan.iter().any(|step| step.contains("TEMP B-TREE")),
            "poll_pending plan sorts out of index: {plan:?}"
        );
    }

    // The on-disk status numbering is frozen: every row in an existing
    // database is a copy of this table, so renumbering a variant silently
    // corrupts it. Never renumber — retire codes and append new ones.
    #[test]
    fn status_codec_numbering_is_frozen() {
        let service: hardy_bpv7::eid::Eid = "ipn:60.3".parse().unwrap();
        let source: hardy_bpv7::eid::Eid = "ipn:60.4".parse().unwrap();
        let timestamp = hardy_bpv7::creation_timestamp::CreationTimestamp::from_parts(
            Some(hardy_bpv7::dtn_time::DtnTime::new(1234)),
            5,
        );

        let frozen = [
            (BundleStatus::New, (0, None, None, None)),
            (BundleStatus::Waiting, (1, None, None, None)),
            (
                BundleStatus::ForwardPending { peer: 7, queue: 2 },
                (2, Some(7), Some(2), None),
            ),
            (
                BundleStatus::AduFragment {
                    source: source.clone(),
                    timestamp: timestamp.clone(),
                },
                (3, Some(1234), Some(5), Some(source.to_string())),
            ),
            (BundleStatus::Dispatching, (4, None, None, None)),
            (
                BundleStatus::WaitingForService {
                    service: service.clone(),
                },
                (5, None, None, Some(service.to_string())),
            ),
            (
                BundleStatus::ForwardAckPending { peer: 7 },
                (6, Some(7), None, None),
            ),
            (BundleStatus::DispatchPending, (7, None, None, None)),
            (
                BundleStatus::DeliverPending {
                    service: service.clone(),
                },
                (8, None, None, Some(service.to_string())),
            ),
            (
                BundleStatus::DeliveryAckPending {
                    service: service.clone(),
                },
                (9, None, None, Some(service.to_string())),
            ),
        ];

        for (status, expected) in frozen {
            let (code, p1, p2, p3) = from_status(&status);
            assert_eq!(
                (code, p1, p2, p3.clone()),
                expected,
                "on-disk encoding of {status:?} must never change"
            );
            assert_eq!(
                to_status(code, p1, p2, p3),
                Some(status),
                "the codec must round-trip its own encoding"
            );
        }
    }
}
